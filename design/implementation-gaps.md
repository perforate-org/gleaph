# Discovered Implementation Gaps

Last updated: 2026-11-02
Anchor timestamp: 2026-11-02 15:24:46 UTC +0000

## Status

**Active tracking document** — this ledger records implementation defects, missing product
capabilities, and contract mismatches discovered while implementing another slice when they cannot
be resolved safely in that slice.

It is not a second roadmap or design source of truth. Each entry names the owning module and links
the active design contract. Once an architectural decision is accepted, the owning design document
or ADR remains authoritative and this ledger points to it.

## Disposition rule

Every counterpartrial gap discovered during implementation, review, validation, or demo integration must
receive one disposition before the current work is committed:

1. **Fix now** when it is a correctness or security defect, blocks the current contract, has a clear
   owner, and can be repaired without obscuring the current slice.
2. **Prerequisite slice** when it blocks the current work but needs independent implementation,
   review, validation, or commit history.
3. **Record here** when it is real but non-blocking, its design is unresolved, or fixing it would
   expand the current slice counterpartrially.
4. **Reject as not a gap** only with evidence that the observed behavior matches an existing active
   contract.

Do not leave a gap only in terminal scrollback, a temporary report, an ignored plan file, or a final
chat summary.

## Entry requirements

Each open entry must state:

- **Observed behavior:** reproducible fact, not a proposed solution;
- **Expected or needed behavior:** the contract or product need that exposes the gap;
- **Owner:** module/domain that owns the violated invariant or missing API surface;
- **Evidence:** test, command, source path, or design section;
- **Impact:** what remains unsafe, impossible, misleading, or inefficient;
- **Next decision:** the smallest question or slice that can resolve it;
- **Status:** `Open`, `Planned`, `In progress`, `Resolved`, or `Not a gap`.

Resolved entries remain in the ledger with the fixing commit and owning test. This prevents the same
defect from being rediscovered without its prior reasoning.

## Open gaps

### GAP-GRAPH-SIX-RED-FACADE-STORE-TESTS — six `facade::store` tests fail against the current contracts

- **Status:** Open. Three of the six are understood (contract drift after ADR 0097); one is a
  compaction behavior question with the spill explanation ruled out; two are untriaged.
- **Observed behavior:** `cargo test -p gleaph-graph --lib` reports 1097 passed / 6 failed:
  `insertion_policy_maintenance_preserves_left_pack_order`,
  `unordered_compaction_swap_publishes_move_and_moves_sidecars`,
  `swap_compaction_rekeys_inline_scalar_index_to_new_slot`,
  `unordered_captured_at_enqueue_drains_without_ambient_table`,
  `updating_undirected_edge_inline_property_updates_both_storage_rows`, and
  `vertex_delete::tests::detach_delete_fence_uses_canonical_forward_slot_for_diverged_directed_in_edge`.
- **What is known per test:**
  - The three swap/left-pack tests sit behind `slab_bucket_allows_unordered_swap`, which requires
    the inline values to be slab-backed and exactly one block per live row; with a value width the
    fixtures do not reach that state before the delete, so the pass left-packs instead of swapping.
  - `detach_delete_fence_...`: the fixture's bucket is **`BucketMode::Tiny`** (`stored_slots = 2`,
    `degree = 2`), and the span step advances past tiny buckets by design, so no slab compaction
    ever runs and the slots never diverge. Confirmed by instrumenting the step entry: it is called
    twice and returns `AdvanceBucket` both times.
  - `unordered_compaction_swap_publishes_move_and_moves_sidecars`: the bucket *is* `Slab` but is
    run-backed - `stored_slots = 0` with a live `edge_spill_run` - so every row lives in the run and
    the span-level swap finds no slots to move. Promoting the fixture out of tiny mode first does
    **not** help: deleting the fillers empties the bucket, which releases its span, so the edges
    under test land in a zero-slot region again and the fold has nothing to fit. A working fixture
    has to keep the bucket non-empty while giving it span room, or seed the slab geometry directly.
  - Seeding it directly was tried and is not the missing piece either: exposing LARA's
    `promote_bucket_to_slab` to the graph crate through a `test-support` feature compiles, but both
    fixtures refuse it with "bucket is already promoted" - the swap fixture's bucket is already
    `Slab` with a zero-slot span, and the fence fixture's is promoted before the seeding point. That
    exposure was reverted rather than kept without a user.
  - The open question is therefore narrower than "the fixture needs promotion": **why does a bucket
    carrying inline values end up promoted with `stored_slots = 0` and every row in its run?** That
    decides whether the remaining swap failures are a fixture problem or an insert-path defect.
  - **Answered (2026-11-03): it is the insert path, by design.** An edge that carries inline property
    bytes is inserted through `insert_edge_skip_leaf_cascade_deferred_inline_property_with_location`
    (`bidirectional/deferred.rs:2140`), which deliberately skips the leaf cascade so the captured
    value offsets stay valid; `insert_edge` - the entry that grows the leaf through
    `rebalance_cascade_after_labeled_mutation` - is never called on that path. Instrumenting
    `insert_edge_inner` showed why every one of those inserts spills: `have_space_on_slab` is
    `loc < slab_window_exclusive_end`, and a fresh vertex's window is empty
    (`stored_degree = 0`), so the first insert already spills and the vertex keeps a run. The rows
    then return to the span only when something grows the leaf - which is what the fixtures' pre-fold
    drain is supposed to do and cannot, because the fold needs the room the cascade would have made.
    A fixture that wants a span-resident value-bearing bucket must therefore grow the leaf by some
    other means first, and that is a real hole in the test kit rather than a stale assertion.
  - **Closed except one (2026-11-03).** The hole is now filled:
    `fixture_support::force_span_fold` (behind the `test-support` feature) runs the eager
    `Required`-policy fold, and the five fixtures that needed a span-resident bucket call it. Suite
    1097/6 -> 1102/1. What remains is
    `updating_undirected_edge_inline_property_updates_both_storage_rows`, whose fixture models exact
    slot placement across several undirected buckets and now differs in one row (slot 2 reports
    `VertexId(2)` where the model says `VertexId(0)`); that one needs its model checked against the
    spill layout rather than another fold.
  - `updating_undirected_edge_inline_property_updates_both_storage_rows` differs in one dumped row
    only, and the discrepancy now looks like a **defect rather than a model gap**. The fixture's
    vertices are `low = VertexId(0)`, `high = VertexId(1)`, `guard = VertexId(2)`; low's undirected
    bucket reads back 17 rows whose values all match the model (`2000..2007`, the `high`-`low` edge
    with `5000` and neighbour `VertexId(1)`, then `4000..4007`), except **slot 2**: its value is the
    expected `2002` but its neighbour reads `VertexId(2)` (`guard`) where every other self-loop on
    low reads `VertexId(0)`. So one row carries the right value and the wrong neighbour - the
    signature of a write landing in the wrong field or a row/value pair being mismatched, not of a
    fixture that forgot to fold.
  - **Reproduced (2026-11-03)** by
    `inline_update_fixture::undirected_valued_insert_sequence_keeps_neighbours`, which replays the
    fixture's insert order and asserts only that each row's neighbour is right. With **8** valued
    self-loops on `high` it passes; with **32** it fails exactly like the real fixture - `low`'s slot
    2 reads neighbour `VertexId(2)` (`guard`) with the correct value `2002`. Removing the guard
    self-loop from the real fixture changed two expectations, so it could not isolate the row; the
    new test can, and the only difference between pass and fail is the size of the *other* vertex's
    first batch. Bisected: **24 rows pass and 25 fail**, so the trigger is that batch's size rather
    than anything about the row being read.
  - What the reproduction shows: `guard`'s own bucket still holds its row correctly (slot 0,
    neighbour `VertexId(2)`, value 6000), so this is **not** a row overwritten by another bucket -
    the self-loop row itself was written with the wrong neighbour.
  - **The payloads are correct at the LARA entry the undirected path uses.**
    `insert_edge_skip_leaf_cascade_deferred_inline_property_with_location` receives
    16 x `(low, low)`, 33 x `(high, high)`, one `(low, high)`, one `(high, low)` and one
    `(guard, guard)` - all right. So the wrong neighbour appears *inside* the deferred placement,
    not on the way in.
  - **Instrumenting the wrong function first cost time, and the mystery was self-inflicted.** The
    earlier probe sat in `lara/edge/insert.rs::insert_edge_inner` and reported 54 calls all with
    owner 0. The reason is in `labeled/graph/insert.rs:540`: this path calls
    `insert_edge_without_logical_slot(&access, VertexId::from(0), ...)` - the `vid` argument is a
    placeholder and the real context rides in `access`. So those counts were neighbours only, and
    they were right (17 x neighbour 0, 1 x neighbour 2, 36 x neighbour 1 against 16 low self-loops,
    33 high self-loops, the cross edge each way and guard's one row).
  - The write site agrees. Instrumenting it (`labeled/graph/insert.rs`, just before
    `insert_edge_without_logical_slot`) shows the exact expected set - 33 `high` self-loops at bucket
    start 4097, 16 `low` self-loops at 4096, one `guard` at 4098, and the cross edge once in each
    direction - so nothing above the row write is wrong, and the earlier "two extra writes" was an
    artefact of comparing runs with different batch sizes.
  - What the addresses do show: **the three buckets' regions start one slot apart** (4096, 4097,
    4098) while each holds many rows, so all of those rows live in the per-bucket spill runs, not in
    the slabs. The corrupted row is therefore a *run* row.
  - **Instrumenting the run write is the wrong layer, twice over.** In
    `log_mut.rs::insert_into_spill_with_layout` every append printed the same neighbour
    (`VertexId(1)`) and the same run id (`0`) regardless of the vertex, which cannot be the stored
    payload: the read-back disagrees. Like `VertexId::from(0)` in the earlier probe, the value this
    layer sees is a placeholder/normalised one and the real identity rides in the `access` context.
    Two probes in a row have now misled on that; the reliable observation is what is read back, not
    what these inner functions are handed. A dump of the run's stored rows (an inspector, not a
    print at a write site) is the next instrument.
- **Expected or needed behavior:** for the swap family, either the fixtures must reach the gate's
  state or the tests must state the narrower contract (value-bearing buckets left-pack until the
  values are dense). For the other three, the behavior must be explained before any test is edited.
- **Owner:** `crates/graph/src/facade/store` (test contracts); `compact.rs` for the compaction
  question.
- **Evidence:** the test names and the run command above; ADR 0052's 2026-11-02 amendment for the
  swap gate; `plans/0408` carries the session log (ignored by git).

### GAP-LARA-INLINE-DELETE-COST — removing from a bucket with inline values is superlinear

- **Status:** Open, measured. Recorded in ADR 0052's 2026-11-02 amendment; the fix is a design
  change, not a constant.
- **Observed behavior:** `plan_bucket_inline_property_bytes_delete` copies the whole trailing value
  suffix into a fresh buffer and writes it back one block left on every removal, with no threshold.
  Deleting a bucket's edges front-first costs 150 K instructions per edge at 8 rows, 234 K at 32 and
  454 K at 128 (width 8), against 124 K for the same drain at width 0 - roughly `k^1.4` across those
  points and 3.7x the width-0 case at 128.
- **Expected or needed behavior:** removal cost should not grow with the bucket. Values are dense and
  ordinal-addressed today, which forces the shift; addressing them by physical slot with tombstones
  would let removal be O(1) and move the copying to compaction, as rows already do.
- **Owner:** `crates/ic-stable-lara/src/labeled/graph/values.rs` (the span) and
  `compact.rs` (the swap gate that depends on denseness).
- **Evidence:** `bench_delete_w0_k128`, `bench_delete_w8_k8`, `bench_delete_w8_k32`,
  `bench_delete_w8_k128` in `crates/graph/src/bench/batch_placement.rs` (commit `e8b4d31c1`).

### GAP-LARA-MATERIALIZE-ADMISSION — Stepped materialization overwrites an already-published value stream

- **Status:**0400 scoped native/component validation COMPLETE under
  [0403](../plans/0403-materialization-validation-completion.md).
  Raw resume/Clone ownership and hash/truncated identity are removed. Shared source/schema/account
  admission and prepared publication distinguish terminal cancellation from retry. Fresh729 native
  and13 feature-tree tests, strict ordinary/feature lib/tests Clippy and the retained materialization
  benchmark pass, with independent narrow approval. Original0398/0399/0400 failures remain immutable.
  Actual timer/upgrade/rollback, public stale-replay reachability and root/selected-codec/cost/adoption/
  0394 remain separate HOLDs; this is not a deployment or performance acceptance.
- **Observed behavior:** On five existing slab targets with width2/fill0xA5 and value accounting10,
  first stepped reserve for width3/15 bytes returns Finished, publishes width3 at offset10 and
  increases accounting to25. A separate valid width1000/5000-byte first step returns position4096;
  inline width2 then publishes a canonical10-byte stream. Resuming the older cursor returns
  Finished, replaces the descriptor with width1000 at offset0 and increases accounting10→5010.
  Each test fails its first post-operation descriptor-preservation assertion. Later value, row and
  exact-error assertions are not reached; these are not successful rejection/cleanup tests.
- **Pre0400 source findings:** `graph/values.rs::materialize_inline_property_stream` checks width/category
  but `materialize_inline_property_stream_step` neither shares that admission nor rechecks it on
  resume. The inline and stepped publishers independently write a descriptor before checked
  vertex value accounting. Numeric offset/length/position carry no consumed allocation identity;
  `retire_byte_span` implements release, not a caller-ownership proof.0399's owning controls are
  bound by [0399](../plans/0399-materialization-reservation-ownership.md) and the separate
  `materialization-reservation-owner-p1/{fixtures.json,results.json,controls.patch}` evidence.
  Both exact commands run1/724 filtered and exit101. Actual fixture budget is1 graph/17 memories;
  both temporary sources are restored exactly and fresh original723/format pass.
- **Lifetime witness correction (0399):**0398's failed setup remains immutable. Its cached source
  slot256 is no longer the canonical source slot after both vertices are built: fresh lookup
  returns261, peer266. The new control verifies both normal source memberships/labels/degrees,
  completes the real5000-byte stream, removes its five edges, verifies accounting0/empty descriptor
  and actual retirement, then reissues offset0 to the peer through the existing allocator.
  All five peer values are0xB2 before replay. Replaying the consumed saved cursor at position4096
  returns Finished and changes904 peer bytes to0x31. Its first byte-preservation assertion fails;
  later descriptor/row/free/error assertions are not reached. This is a direct-core lifetime
  counterexample, not a general deletion defect or a reached public-wrapper replay.
- **Queue identity witness (0399):** Two initial materialize items with identical vid1/slot7 but
  Forward/Reverse owners and distinct fills produce the same key `(2,7,2117564854)`; enqueue retains
  only1 item. Exact-key inequality fails first; later two-payload preservation assertions are not
  reached. `work_item_discriminator` omits orientation and truncates the physical slot to u32.
  A collision-free reservation owner cannot be inferred from this key. The high-slot truncation
  is source-inspected, not another executed native case.
- **Pre0400 queue correction:** Fresh `bidirectional/deferred.rs` inspection finds that a materialize
  error sets `stalled`; the outer worker requeues the same item at Retry and stops the pass.
  It does not drop it. A continuation uses the work item's natural Routine priority, whereas
  initial interception enqueues at Retry. Sibling drain/re-resolution/synchronous materialize
  drain remain relevant. This corrects the drop premise in0397's immutable specification and
  the entry0398 draft; no reservation cleanup may rely on that premise. At that checkpoint,
  invalidation/reuse, cancellation and retry still needed owning evidence.0400 supplies the scoped
  native owner/queue/wrapper regressions below, not actual public canister lifecycle proof.
- **Expected behavior:** At the existing values owner, normal/source/bucket membership, width,
  stream extent and checked accounting must be admitted before reserve/fill/publication and
  reconciled on each resume. A rejected reservation may be retired only with proof that the
  cursor owns that unpublished allocation and that a retained retry cannot write a released
  range. Keep successful wrapper drain/retry behavior; no guessed free, epoch or full-graph guard.
- **Owner:** `ic-stable-lara` labeled values admission/publication and deferred queue cursor
  consumption/retry. The values allocator continues to own physical reservation/retirement;
  root/cover/pin and record/header codec replacement are separate dependencies.
- **Evidence:** [0398](../plans/0398-materialization-admission-native-gate.md),
  [the planned0397 ownership specification](../plans/0397-vertex-owned-span-design-gate.md), and
  [0384's publication scope](evidence/adr0097-bucket-segment-publication-preflight.md).
  Under the ADR0097 evidence root, `segment-implementation/tiny-leaf-geometry/geometry-copy-composition/
  materialization-admission-native-gate-p1/{fixtures.json,native-results.json,probe.patch,disposition.md}`
  bind actual source, three graphs/48 memories and all three exact terminal receipts. Each command
  runs1 test/725 filtered and exits101. Temporary probes are archived and restored byte-for-byte;
  fresh original native723 pass. No production Rust, existing test or persisted layout changes.
- **Current repair:** [0400](../plans/0400-materialization-owner-implementation.md) owns one shared
  admission and prepared single-descriptor/vertex publisher. Opaque pending cursors persist as V2
  only; consumed cursors reject replay and encoding. Fill/retirement errors retain the same reservation,
  successful cancellation consumes once and surfaces its original error, and wrapper publication is
  verified rather than inferred from queue absence. The15-byte exact key includes orientation/vid/
  full slot; dedup excludes priority and preserves existing progress. Dense→span continuations now
  use the same key factory instead of inheriting the completed Dense tag. Fresh state/reinstall
  is required; no key/V1 fallback, new catalog, epoch or MemoryId is introduced.
- **0400 validation history:** Its canbench-feature Clippy reported8 inherited diagnostics in
  bench.rs, traverse/bench.rs and bench_tree.rs. The focused benchmark built Wasm but install failed before
  measurement: method-name sum20082 exceeds20000. Wasm export parsing binds new benchmark85 and
  inherited exports19997 bytes. Only3 bytes remain, so shortening one new name cannot solve it.
  At that checkpoint the new benchmark was archived/removed and bench.rs restored exactly; no
  existing name/artifact or lint was changed/suppressed. Evidence: `materialization-owner-implementation-p1/{canbench-export-
  diagnosis.json,benchmark-disposition.json,unexecuted-benchmark.rs}` and complete command receipts.
  No0400 materialization instruction/performance result exists in these original receipts.
- **Prerequisites now passed:** [0401](../plans/0401-canbench-export-retirement.md) retires ten
  obsolete/duplicate exports while preserving their oracles and historical results.
  [0402](../plans/0402-canbench-feature-lint-repair.md) mechanically repairs all eight benchmark
  diagnostics without changing callbacks, fixture cardinalities, assertions or registration names.
  Strict ordinary/feature Clippy,729 native tests and13 feature-tree tests pass; seven existing
  fixture measurements complete. The registration set remains560 names/19,187 bytes with813 bytes
  free. This is a lint/export prerequisite checkpoint, not0400 benchmark or performance closure.
  Its prerequisite checkpoint itself does not close0400;0403 supplies that separate validation.
- **0403 scoped completion:** The five production/test files remain byte-identical to0400's approved
  r2 binding. The original benchmark definition is retained verbatim with the same two graphs/32
  memories,600 rows each and4096/704-byte steps; one focused canbench install/query reports30.02 K
  instructions for the4800-byte component. Actual exports are562 names/19,272 bytes,728-byte headroom;
  only its two85-byte names are added to0402's560-name surface. Persisted benchmark artifacts stay
  unchanged. A stale723-test artifact from0402's index-only snapshot was rejected; mtime-only
  owning-source invalidation and an exact729-identity rerun supply the accepted native receipt.
  Evidence: `materialization-validation-completion-0403/{baseline.json,commands.json,
  native-cache-diagnosis.json,exports.json,review-binding.json,review-report.md,
  review-completion.json,checkpoint.json}`. No original failed receipt/checkpoint is rewritten.
- **Impact / next decision:** Scoped native values/queue/wrapper validation is complete, not a
  performance comparison or full lifecycle/adoption decision. Actual timer/upgrade/rollback and
  public stale-replay reachability are not claimed by native queue reopen. Root repair, selected codec,
  workload costs, adoption and0394 remain separately gated.0397's error-drop premise remains false.

### GAP-LARA-TINY-GEOMETRY — Tiny promotion publishes before complete geometry admission

- **Status:** Planned / STOP at tree-resident source contract; the shared-publisher probe is
  rejected and restored, not implemented. The physical0395 and
  [mode-footprint0396 prerequisite](#gap-lara-mode-footprint--normalbypass-handoff-corrupts-assigned-pin-width)
  are closed. The post0396 baseline/four fresh BEFORE controls remain bound by
  `W/geometry-copy-composition/restart-p2-post0396-baseline/{baseline,before-results}.json`.
  Separately authorized restart P3 reproduces both owning REDs, then passes three candidate functions
  but regresses eight existing successes (native726:718 pass/8 fail). Its actual written fixtures are
  11 graphs/176 memories/two corrupt-index reopen attempts; the other ten functions, caller's eight
  valid reopen checks and full13 fixture freeze remain pending. All eight changed Rust paths and
  temporary contract/diagnostic suffixes are archived and restored exactly; native723, ordinary
  lib/tests Clippy and workspace format pass after restoration. [0394](../plans/0394-grouped-tiny-leaf-geometry.md)
  completes only freeze-and-prove-order; four TODOs/eight criteria, group integration, mutants, AFTER
  and independent implementation/cost closure remain pending. Current723 plus13 planned still implies736.
  Q1 draft approval and every prior RED/BEFORE/AFTER retain their original producer/scope.
- **Tree-resident source blocker (restart P3):** An exact mixed-tree caller has pin=None,
  leaf actual/total0/0 and vertex stored_slots0 despite an initialized one-slot root. A separate real
  slab/run/property-tree promotion leaves slab[256,272), cover[256,273), pin[256,1280), but combined
  root[1280,1290). `promote.rs::transcribe_and_publish_tree` allocates a standalone root; its normal
  branch keeps new_alloc=old_alloc and publishes only `TreePublication::Existing` descriptors, not a
  final vertex cover. The bypass branch has a distinct segment/vertex image. This is not0396's
  repaired assigned-width drift. The normal root/cover/pin publication contract needs a bounded
  prerequisite or reviewed0394 rescope before geometry can enforce containment and retain existing
  success. That owner lies outside the20-path permission; no repair, guard relaxation, skipped mate,
  sequential publishing rewrite, counter reset or test weakening is authorized. New
  `W/geometry-copy-composition/restart-p3-geometry-implementation/{rejected-probe,source-restoration}.json`
  and `tree-resident-source-diagnosis.md` preserve both exact reached states, eight failures, candidate
  source and restoration; the other six individual failure sources are not independently diagnosed.
  [0404's separate shared-leaf root repair draft](../plans/0404-shared-leaf-tree-root-ownership.md)
  inventories the symmetric append/property-grow/deepen/flatten geometry and demotion retirement
  consumers at the existing compact owner. Demotion's direct root-subrange releases cannot remain
  unchanged if roots move into a live pin; its geometry handoff must be reviewed without silently
  reopening GAP-2026-09-22-001/003's deletion/value/movement policy. The unpinned P3 batch witness
  uses `force_tree_mode_for_test`, so it does not prove naturally produced run-only promotion.
  0404's r1 independent draft review requested changes at the unpublished demotion value-span
  handoff. The inspected immediate allocator publishes free-prefix removal or occupied-tail growth;
  `release_byte_span` is deliberately a no-op within that tail, so allocation followed by that release
  cannot prove discard. The revised draft explicitly selects a nonpublishing destination at the
  existing byte-slab owner, reusing source-bound free-span preparation and composing its header,
  bytes and acquisition into the compact unit. Prefix/append discard, changed-source rejection,
  late root-admission failure and supported property conversion have owning proposed assertions.
  This is a proposed extension, not an existing API or newly executed leak proof; r1 is retained and
  r2's verdict/source binding is recorded separately. Existing ascending conversion positions and
  movement behavior stay unchanged; property-tombstone selection, movement parity, deeper-property
  retirement and deletion/liveness remain GAP-2026-09-22-001/003, not certified by this rescope.
  Both natural RED obligations remain incomplete. The approved root-repair design remains planned;
  the bounded Step2 source candidate below is not a root repair,0394 restart,codec,cost or adoption approval. Evidence is under
  `shared-leaf-tree-root-ownership-0404` in the ADR0097 evidence root, including
  `review-report-r1.md`, `demotion-handoff-stop.md` and the r2 revision queue; old P3 receipts are unchanged.
  R2 approves the draft boundary only. The next bounded `implementation-freeze-p1` checkpoint
  separately freezes dirty-entry bytes/identities and the first normal-promotion contract:4 natural
  width0/33, target-before/after-retained-label fixtures,64 memories,4 conditional reopens. It does
  not allocate production edits or infer the unassigned run-only case from a pinned/forced source;
  both natural RED obligations must be established before the root publisher is changed. Runtime
  reach/containment receipts and pending obligations are recorded under that checkpoint, not inferred
  from its729/13 entry identity lists. The initial pre-compilation `--locked` failure remains immutable.
  User-authorized config isolation moved the exact sole global `creusot-std` patch to Creusot-local
  `.cargo/config.toml`; `implementation-freeze-p1/config-isolation/{permission,application}.json` binds
  that move, unchanged manifests/locks/index and the same-command compilation reach. The original
  external config was not frozen at entry, so its earlier change time is unknown. No unlocked retry,
  home/target override or lock write; the earlier GAP-WORKSPACE-LOCK-DRIFT resolution is unchanged.
  The remaining blocker is the normal fixture's source preparation: an initial owner-wide fold/peer
  source lost retained rows; r2 without that fold reached both width0 promotions but then stopped on
  width33's zero prefix. A unified seed helper at the existing run-to-prefix compactor again loses
  retained rows to peer values before promotion. D1 now proves the earlier source violation, before
  any peer insertion: cover[256,293), last start288/prefix4, four run rows need end296. Shared
  `bucket.rs::bucket_successor_start_after_bucket` admits325 (last start + whole vertex width37);
  `compact_label_bucket_overflow_to_slab` writes292..296 and publishes prefix8 without extending the
  cover. The second copied row293 is already outside the reservation. This is a production source-tail
  defect, not permission to resequence fixtures or label a setup failure as root RED.
  `implementation-freeze-p1/source-diagnosis/finding.json` binds exact before/after canonical metadata,
  probe/source hashes and audited wrapper/scalar/batch consumers. A subsequent complete `crates/`
  call-site audit finds that the second, supplied-image contiguous-copy predicate has only its
  declaration and no caller. The revised prerequisite deletes that unused duplicate instead of
  extracting a helper to preserve dead code; one existing successor owner survives. The own-prefix
  slab window and new-descriptor anchor have different contracts and stay unchanged. User `いいよ`
  separately allocates the tail exception under `source-tail-prerequisite` (S). Independent draft
  review approves that revised boundary, not completion. Entry730/13 identities are re-listed; one
  written1-graph/16-memory/1-reopen candidate adds a731st native identity but fails before its owning
  scalar assertion. At that previous checkpoint no production method was deleted/fixed and both
  natural root RED obligations were incomplete. Root writers remain read-only. The authoritative boundary is0404's source-tail
  subsection; no root repair/adoption permission is inferred.
  **Additional position/admission prerequisite:** the candidate's retained leading Tiny label2 and
  sole slab label3 fail the first growth at eight slab edges: prefix4/run4, reservation5,
  cover[256,261), pin[256,1280), leaf counts8/1024. The existing helper returns exact
  `Store(CollectAllocationOverflow)` with the observed vertex/pin/count fields unchanged; full
  backing-byte preservation on this error was not proved. A30-second pre-runtime compiler timeout
  is separate from that completed failure. Diagnostic source/terminal and the inspected model are
  bound in `S/preparation-stop.json`; temporary PREP prints are archived/removed. The only non-tiny
  is also the queried bucket, so old/proposed tail origins are identical: this is not the last-origin
  RED. Do not resequence Tiny/growth or weaken the source guard to bypass it.
  The then-current `compact.rs::calculate_label_edge_span_positions_by_resident_slots` gave[256,262]
  for the captured growth request(base256,width37,preferred1,extra4): Tiny has zero resident width but
  advances its weighted gap. This conflicts with `PreparedVertexPhysicalCopy::reserve` requiring
  first non-tiny==placement.start256; the ready guard is correct and must remain. That static
  incompatibility is established; the exact first runtime Err site was not separately instrumented.
  The older calculator has no production caller; its only call is the existing init overflow test.
  User `いいよ` now allocates the concrete compact position/first-origin amendment, preserving that
  unchanged test via a test-only forwarding entry while deleting its duplicate algorithm. One pure
  origin assertion in the same natural fixture must precede growth and production edits. The exact
  amendment is in0404 and `S/position-{permission,freeze}.json`; it retains the cumulative590-second
  ledger and requires its own independent review. All other compact/root/pin/count/value/codec
  boundaries remain protected. Tail draft approval does not cover this amendment. At that earlier checkpoint neither intended
  scalar RED nor GREEN/mutant/reopen/normal-matrix replay had been reached.
  **Current allocated proof/status:** independent `APPROVE0404 POSITION DRAFT`, then pure origin
  RED[256,262] vs[256,256], precedes the compact position change. The duplicate algorithm is deleted;
  its unchanged init overflow test forwards through a test-only entry to the resident owner. Leading
  Tiny anchors now preserve the first resident origin; ready/cover guards and other compact code
  remain unchanged. The unchanged growth reaches scalar tail RED325 vs293. Deleting the unused
  tail predicate and fixing the surviving tail owner passes exact rejection/rows/counts and writable
  reopen. The old-tail mutation fails325 vs293; exact restoration passes. The unchanged overflow
  control passes. The original four-case candidate now reaches all natural promotions and its final
  root containment RED; root reopens remain unreached.730 native identities pass with only that
  intentional root identity skipped, plus13 feature-tree identities. Source/terminal/restoration
  bindings are `S/position-tail-runtime-facts.json` and associated logs.
  At S's stop Clippy was blocked solely by the now-unused `graph.rs:235` facade re-export of
  `bucket_resident_region`, whose former qualified consumer was the deleted algorithm. The actual
  helper remains live in compact. At S's stop `graph.rs` was outside the allocated surface and unchanged;
  removing only the obsolete export needed separate allocation/review. No artificial reference or
  lint suppression is introduced. The implementation-review command times out(exit124), despite a
  complete advisory `REQUEST CHANGES` response in its terminal; no approval/successful review gate
  is accepted. Final formatting also times out(exit124); earlier formatting is not a final-source
  pass. Neither was retried or used to reset S. User `いいよ` now allocates the one-item export cleanup
  and remaining validation under `C=E/source-tail-export-cleanup`. S's552.9174312460236/590-second
  ledger and receipts stay immutable. C owns only that graph.rs line and this status/0404 plan;
  bucket/compact/root code and all test bodies stay byte-identical. The one-item cleanup has independent `APPROVE0404 EXPORT DRAFT`; the obsolete export is now
  removed and every other graph.rs byte/implementation/test body stays unchanged. Final-source
  Clippy/format and730 native successes(skip only the intentional normal-root RED)/13 feature
  successes pass; the unchanged normal contract replays all four successful promotions and its
  final containment RED. `C/pre-review-preservation.json` verifies81048 prior protected files plus
  every S artifact, exact unowned repository bytes and HEAD/index/lock. Independent review exits0
  and returns `APPROVE0404 EXPORT IMPLEMENTATION` with no P1/P2 findings; packet/report/source
  bindings are in `C/implementation-review-binding.json`. This source-tail/position/export
  prerequisite is CLOSED, with final status/preservation bound in `C/checkpoint.json`; whole0404
  is not completed. No new test/graph/memory/reopen or runtime/public/persistence behavior is
  introduced by the cleanup. Old failures/timeouts remain unchanged and are not credited as passes.
  Natural unassigned run-only RED is still missing, and root production/0394/codec/cost/adoption
  remain HOLD.
  Temporary prints are archived/removed. Before the source fixes, the durable containment guard
  reproduced resident[288,296) outside cover[256,293), before peer/bulk/removal/promotion/reopen.
  Those failed sources/censuses and partial reaches stay separate. The earlier F attempts constructed
  7 graphs/112 memories, with only r2's two partial width0 promotions and zero reopens; those compiled
  730/13 identity lists were not runtime passes. The separate S/C receipts now establish the owning
  GREEN/reopen and full four-case natural normal RED. Natural unassigned run-only RED and a separate
  root-source checkpoint approval remain unachieved. No root repair or0394/codec/cost/adoption closure
  is claimed.
- **Observed behavior:** Source tracing of `insert.rs::promote_tiny_to_slab` shows publishing
  `set_elem_capacity`, first-pin allocation or a mate's reduced `stored_slots` before later
  checked transcription, descriptor writes and PMA count-index work. `leaf_pin.rs::ensure_labeled_leaf_block_pinned`
  allocates, publishes `physical_start`, then calls the fallible count update. Independent same-leaf
  calls use canonical occupancy/counts rather than one combined prospective image.
- **Additional source findings:** The promoter's target/mate content-end loops use
  `stored_slots_raw` for every non-tiny bucket, although tree residency is its combined LEG roots,
  owned by `compact.rs::bucket_resident_region`. The promoter also appends a newly promoted bucket
  at content end. If its label sorts before the existing first non-tiny descriptor, that descriptor
  becomes the inferred cover anchor without moving older resident bytes. The source therefore
  does not establish the existing first-non-tiny cover invariant for that shape. Native proof now
  confirms all four real callers (insert, inline materialize, stepped materialize, compatible
  width-on-slot) succeed in both label orders with exact topology/schema/global/leaf counts, but
  each later-first case strands resident[256,260) before final cover[260,269). Singleton promotion
  returns Ok rather than the planned append-only rejection. The pin proof returns exact
  SegmentCountsTreeTooSmall after capacity256→272 publication. Later preservation assertions are
  not reached in those REDs; no data-loss/reopen/frequency/performance claim is made.
- **Physical prerequisite admission gap:** `leaf_pin.rs::try_expand_labeled_leaf_in_place` consumes
  the exact adjacent prefix immediately through `FreeSpanStore::take_prefix_at`; the existing
  prospective `prepare_replacement` chooses best-fit only.0395's physical-only unit must preserve
  this source identity with a minimal exact-prefix prepared entry and shared selected-prefix split
  projection at FreeSpanStore, not substitute a nonadjacent span or publish acquisition early.
  Source hashes and correction scope are bound in `W/prerequisite-r4-prefix-admission-gap.json`.
  The allocated0395 allocator checkpoint implements that choice through the single private
  physical replacement entry, without changing allocator layout/index. Exact-prefix/free-hole
  tests pass; the unified physical-only compact rewrite/leaf-slide consumers now join that loan.
  Tiny first-pin/grouped promotion callers do not;0394 remains stopped.
- **Expected behavior:** The existing geometry owner must prepare one same-leaf placement/count
  image, validate final cover/physical residency and all writer/allocator admission, then consume
  an error-free publisher. Reject unsupported/full/overlapping geometry before canonical mutation.
  [The approved0384 protocol](evidence/adr0097-bucket-segment-publication-preflight.md#planned-tinyleaf-geometry-0394)
  remains authoritative for bounded width preparation; the ledger does not add width semantics.
- **Owner:** `ic-stable-lara` labeled tiny promoter/leaf-pin geometry; descriptor, vertex, edge,
  span-meta, count and allocator stores continue to own their writer/address/encoding contracts.
- **Evidence:** `crates/ic-stable-lara/src/labeled/graph/insert.rs::promote_tiny_to_slab`,
  `leaf_pin.rs::{ensure_labeled_leaf_block_pinned,labeled_leaf_occupied_spans}` and
  `compact.rs::bucket_resident_region`; source hashes and current dirty baseline are bound in
  `segment-implementation/tiny-leaf-geometry/{baseline,implementation-baseline,red-stop}.json`
  under the ADR0097 evidence root. `red-proofs.rs`/`red-proofs.patch` preserve the temporary test-only
  addition; `tiny-leaf-red-pin-count.log`, `tiny-leaf-red-earlier-anchor.log` and
  `tiny-leaf-stop-current-callers-width0.log` are exact1-test failed terminal receipts. The initial
  nonempty width2 fixture is separately preserved as an intentional width rejection, not stop
  evidence. The source suffix is restored byte-for-byte; no production geometry changed.
- **Impact:** Native returned errors are not yet proven to preserve prior pin/mate/descriptor/count
  state. Hoisting only scalar checks cannot admit two requests sharing the same physical leaf;
  using logical tree width or changing the inferred anchor can invalidate cover/release ownership.
- **Next decision:** Historical r3/r4 NOT APPROVE reports/queues remain immutable. Completed
  focused r5 (exit0/116.36s, zero tools) approves the revised0395 DRAFT boundary and satisfies both
  retirement-owner/hole-success findings plus prior capability/slide/test/prefix obligations.
  `W/prerequisite-review-r5-completion.json` binds the exact reviewed packet/diff/source scope.
  Implementation is allocated; the dirty baseline,18-path manifest, retained Wasm and all three
  BEFORE controls are frozen. `FreeSpanStore::prepare_physical_replacement` is now implemented
  with one guard, original-source hole subtraction, shared prefix/coalescer admission and ready
  witnesses. Its allocator portion passes native709 (allocator34/bucket17 retained) and strict
  Clippy, including fixed retire/statistics, exact-prefix versus best-fit, publisher1/probes0 and
  changed-hole/neighbor publisher0 assertions. The subsequent buffered slab owner checkpoint
  replaces the old single-range constructor with `EdgeSlabStore::prepare_physical_image`, checking
  all buffered ranges/backing before writes and installing one header. Its partial backing test
  reaches real grow rejection, reserve/drop preservation and a premature-write counterexample;
  native710 retains allocator34/bucket17 at that intermediate checkpoint. The subsequent physical
  graph checkpoint implements the nonpublishing desired copy and sole canonical vertex/leaf ready
  publisher; all seven identities now exist. Ordinary rewrite, streamed relocation and overlapping
  slide share complete admission, one header/count and guarded retirement; duplicate finalizers and
  per-vertex slide deltas are removed. Native715, strict Clippy/format and restored mutation pass.
  Historical r4 AFTER was slab1,530,362/tree1,577,466 instructions; current I2 source-bound controls
  are slab1,531,031/tree1,578,135/prefix1,048,658, growth0. Native replay supplies read/write and
  retained1536/2048-byte observables, not peak heap/system-call tracing. I2 reproduces and fixes
  unpinned overlap retirement of newly live destination slots, makes singleton metadata admission
  fallible and preserves canonical ordinal ties with allocation-free sorting. Its bounded tests
  prove exact retire2slots, reopen and reuse without new graph/memory/identity (23/368, reopens14).
  Completed independent I2 ends APPROVE IMPLEMENTATION ONLY, no actionable implementation/doc P1/P2.
  Prefix +20,730/+2.0167% versus BEFORE, unchanged after I2, is separately user-accepted with
  `そうしよう` as a known shared-implementation/safety trade-off, not proof all instructions are
  indispensable or inherited0386 tolerance. No new allocator/map/layout, second physical publisher,
  tiny/width integration or outer-operation guarantee is claimed.
  `W/physical-layout-implementation/physical-layout-user-cost-acceptance-u1.json` binds the explicit
  decision; `physical-layout-final-closure-u1.json` binds final plan/source/doc/validation/preservation.
  Earlier `{allocator,slab-image,physical-publisher}-checkpoint-results.json`, r4/T3 and incomplete I1
  remain immutable with their historical scopes; no earlier receipt is rebound to current I2.
  Do not use sequential publishing rewrite, minimum-address cover or unsafe fallback.0395's
  physical-only prerequisite is CLOSED; this tiny-geometry gap and0394 stay STOPPED IMPLEMENTATION.
  Q1 now drafts required earlier-label copy success, real retained-prefix/root scratch, generic first
  pin and one final logical geometry/header/count/pin/allocator unit through the existing compact
  admitted publisher. Physical-only factories stay restricted and desired copies alone cannot mint
  ready. Baseline715/planned728 preserves all0395 tests. Correct the misleading fourth-inline-insert
  control into same-length `tiny_promote_d4_1024` before any production geometry change, freezing
  new BEFORE/export/install sanity with no extra methods or old-result rewrite. New
  `W/geometry-copy-composition/rescope-q1-baseline.json` binds the four-path planning revision and
  immutable0395 closure; `rescope-review-q1-{report.md,completion.json}` in that directory now binds
  completed draft approval, not implementation/runtime/performance/0383 approval. The subsequent
  `0394を再開して`/`OK、進めて` explicitly allocated restart P1; its new baseline binds HEAD f25567bad,
  native715 and the20-path surface. Both new REDs reproduce; corrected K4 BEFORE is190,825,723
  instructions with physical controls1,531,031/1,578,135 and export bytes19997. `restart-p1/before-results.json`
  binds those original sources/Wasm/CSVs; no AFTER/cost acceptance is implied.
  P1 is now STOPPED: its first-pin shared-publisher probe greens the new count-rejection contract but
  regresses the unchanged `public_capacity_bucket_reservation_failure_preserves_cached_peer`.
  Original and candidate pin entry both reach start256/PMA actual69,total80/capacity272; original
  exact1 passes, candidate infers end336 and rejects. `restart-p1/{rejected-pin-probe,baseline-pin-problem}.json`
  bind source/terminal diagnostics. This is a baseline inference conflict, not a fabricated corrupt
  fixture or permission to remove bounds. Leaf physical range, compact physical source and edge
  row-window inference must share the checked assigned-range source. R1 now diagnoses the writer
  equation `16−5+(5+64)=80`, with unchanged pin/capacity/prefix; see
  [GAP-LARA-MODE-FOOTPRINT](#gap-lara-mode-footprint--normalbypass-handoff-corrupts-assigned-pin-width).
  These mode-writer and outer-relocation decisions are outside Q1; current persisted fields can
  represent the correct source and no additional pin metadata is selected. Do not reset counts, enlarge an unadmitted extent,
  pass through unchecked geometry or weaken the existing success test. The production probe and
  temporary RED/diagnostic suffix are archived/restored byte-for-byte; only the benchmark correction
  is retained.0395's closure is not reopened;0394 needs reviewed source-owner prerequisite resolution,
  not repeat implementation allocation. Its mixed-mode/group order, real caller GREEN/mutation,
  cost and implementation closure remain unproved. Queue/Graph/live width/0383 stay later gates.

### GAP-LARA-MODE-FOOTPRINT — Normal/bypass handoff corrupts assigned pin width

- **Status:** Resolved in the validated dirty worktree by [0396](../plans/0396-mode-footprint-handoff.md),
  with P5 independent implementation/cost approval and final closure. Fixing change:
  `fix(lara): preserve assigned pin width across mode changes`, parent f25567bad;
  `mode-footprint-user-commit-c1/commit-receipt.json` binds the exact commit and index-only validation.
  The owning regressions are `mode_footprint_retained_roundtrip_preserves_assigned_pin` and
  `mode_footprint_tree_append_preserves_pin_and_retirement`.0394's fresh baseline is now frozen.
  Draft approval, original REDs and P3 BEFORE bindings remain immutable.
- **Observed:** The existing retained-prefix/run fixture reaches physical prefix5 and assigned
  pin16 at256. Normal→bypass subtracts5; after spill reaches64, bypass→normal charges raw69 as
  normal cover and adds69 to total11. Pin/capacity are unchanged and the inferred end336 exceeds272.
  The strengthened temporary exact1 assertion fails, exit101. Its diagnostic source is archived
  and restored; the original success contract is unchanged.
- **Owner:** `graph/insert.rs::enable_default_edge_bypass`,
  `graph/bypass.rs::promote_bypass_to_bucket_mode` and the tree-append sibling
  `graph/promote.rs::promote_bypass_with_edge`; `EdgeStore` and its counts/row-layout owners enforce
  prepared physical contribution/range decisions. Segment rewrite owns descriptors/final row links.
  Existing prefix/root projection owners remain authoritative.
- **Evidence:** [Source diagnosis, current representations and proposed boundary](evidence/adr0097-pin-extent-source-diagnosis.md).
  `W/geometry-copy-composition/pin-extent-source-r1/{baseline,stage-cause}.json` plus
  `stage-red-source.rs`/`stage-red.patch` and `pin-extent-source-r1-stage-red.log` bind four reached
  stages and the source. One existing graph/33 memories; no new identity/PocketIC/install/reopen.
- **Historical impact:** The defective writers manufactured pin slack, truncated its inferred extent
  and published a cover containing spill width. Strict admission rejected an existing caller. The old
  mixed-mode audit skipped total and missed intermediate11. No data-loss claim follows from this probe.
- **Implemented ownership:** `EdgeStore::prepare_vertex_span_counts` selects assigned retention or
  exact unassigned contribution for compact/ordinary/tree paths. One checked count-source read admits
  backing before capture/preparation/revalidation. The segment publisher owns the final bypass row
  without clear→set; the tree transcriber owns prepared actual exclusion and the exact post-allocation
  header witness. One checked assigned-range interpretation replaces the inspected reader formulas.
  Current formats, clean scans and root placement/retirement policy remain unchanged; no clamp,
  count reset, additional length field or compatibility path is introduced. Fresh state is required
  for old corrupt counts/covers. P5 deletes both fallible tree publication callbacks and preserves the
  existing two writer forms in private transient prepared/ready sum types. Storage reserve/ready
  precedes one final transcriber admission. The same6-case fixture injects during descriptor reserve,
  preserves active source LTB0, checks exact free targets1/2/links/kinds and exact logical-row/allocator
  integrity at the existing rejection reopen. Bypass audit bounds now use initialized prefix, not spill.
- **P4 validation:** Native723 passes with all715 original identities and8 additions. Frozen fixture
  cost is16 fresh graphs,1 outer graph,290 native memories and8 reopens,0 PocketIC/installs. Retained,
  mixed, unassigned, empty/zero-live/run ownership, index/arithmetic/backing, stale-source, eligibility
  and allocator-grown tree success/cleanup variants pass. Three exact mutants (assigned charging,
  raw spill-inclusive cover, late count admission) fail at their advertised assertions; byte-identical
  restoration and the same exact tests pass. Strict lib/tests Clippy, format and locked Wasm build pass.
  The zero-live prefix expectation is intentionally retained rather than discarded; the existing
  relocation fixture seeds a real folded prefix while keeping exact67 rows and both-order assertions.
  `W/geometry-copy-composition/mode-footprint-p4/{fixtures-r4,native-validated}.json` and
  `mutation-*-{applied,restored}.json` bind these stages. Failed RED/build/first-native commands stay failed.
- **P5 continuation:** User `いいよ。進めて` authorizes a separately bound runner admitting only R/crate
  and four fresh external AFTER directories. It derives the immutable real-lock contract; protected
  old runners/configs/CSVs and repository saved results remain unchanged. Native723/Clippy/format,
  the late-tree-admission mutant/restored exact GREEN and fresh locked Wasm pass. Fresh AFTER versus
  P3 BEFORE: slab1530725 (-306), tree1577829 (-306), tiny191032946 (+207223/+0.1086%), bypass18854 (0),
  all page growth0. Independent re-review accepts the tiny pressure-retry increment as bounded
  safety-admission cost; no weakened sanity or0395 cost approval substitution. `mode-footprint-p5/{baseline,
  runner-permission,fixture-red-r2,native-validated,mutation-restored,after-results}.json` binds this.
- **Closure:** P4's packet-only REQUEST CHANGES (148.22s, one turn/ZERO tools) stays recorded. P5
  re-review returns APPROVE IMPLEMENTATION AND COST BOUNDARY (45.80s, one turn/ZERO tools), no
  remaining concrete P1/P2. `mode-footprint-p5/{review-completion,pre-closure,closure}.json` binds the
  verdict, unchanged reviewed code, all required gates and exact source/protected/index preservation.
  These are pre-commit receipts; subsequent user authorization permits the scoped commit, not push.
  0394's separately authorized baseline-only restart now freezes native723 and fresh BEFORE in
  `restart-p2-post0396-baseline/before-results.json`. The current full-worktree Wasm has19997
  export-name bytes; a cached14657-byte C1 snapshot binary is rejected before measurement and the
  owning compilation unit is rebuilt. Production source, all410 protected hashes and empty index
  are preserved. No geometry/runtime approval or reinterpretation of historical715/728 follows.
- **Preflight:** The historical P2 locked-build stop is resolved by P3's separate
  [workspace lock prerequisite](#gap-workspace-lock-drift--locked-build-prerequisite-resolved).
  P3's four fresh BEFORE controls are retained as BEFORE; P4 native/Wasm are not benchmark passes.

### GAP-WORKSPACE-LOCK-DRIFT — Locked build prerequisite (resolved)

- **Status:** Resolved by user-authorized P3 lock-only alignment, before mode production changes.
  No manifest/source/version upgrade. P3's no-stage/commit/push receipt remains historical; the
  subsequent user-authorized mode-fix commit includes the reviewed lock. Lock alignment alone does
  not resolve the mode defect.
- **Historical P2 observation:** `env CARGO_TARGET_DIR=target/canbench cargo build -p ic-stable-lara
  --locked --release --target wasm32-unknown-unknown --features canbench` failed before compilation
  because Cargo needed to update `Cargo.lock` (exit101/1.08s). Locked target metadata also failed
  (exit101/1.29s). These failures remain immutable.
- **Owner:** Workspace manifest/lock consistency; `Cargo.lock`, workspace/package manifests and
  dependency resolution, not `EdgeStore` or the mode-conversion publishers.
- **Evidence:** `W/geometry-copy-composition/mode-footprint-p2/baseline.json` freezes the continuation.
  `lock-diagnostic.json`/`lock-delta-summary.json` record successful offline resolution in a separate
  diagnostic mirror:1227→1083 lock packages, morph-dict gains unicode-normalization, obsolete local
  graph-explorer-new is removed, GPUI disambiguation changes; no changed canbench/ic/LARA package rows
  are observed. The P2 real lock had SHA256 `15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`.
  `minimal-lock-diagnostic.json` shows that adding only morph-dict's dependency to another diagnostic
  copy still fails locked metadata (exit101/1.35s); a one-line canonical repair is not proven.
- **P3 resolution:** User `いいよ` authorizes the lock exception. Independent r3 packet-only review
  approves only its boundary (exit0/13.50s/ZERO tools). Cargo's real offline resolution exactly matches
  reviewed SHA256 `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`:
  144 removals/zero additions, retained versions/sources/checksums equal, and optional-inclusive168-package
  LARA closure equal. Six obsolete optional-edge reductions and morph-dict's current manifest edge are
  explicitly recorded. Original lock/evidence/measurements remain historical; no handwritten schema,
  cargo update, manifest edit or unlocked build is used.
- **Validation:** Locked real target metadata passes (5.07s), the original locked Wasm build passes
  (128.76s), and all four fresh BEFORE controls pass with19997/20000 export-name bytes. Exact current
  source/lock/Wasm/CSV/native evidence is `W/geometry-copy-composition/mode-footprint-p3-lock/
  {application,locked-target-metadata-receipt,semantic-lock-delta,lara-lock-closure,before-wasm,before-results}.json`.
  Old715 identities pass with only the reproduced deliberate RED excluded; no all-tests GREEN.
  Repository saved results/index and all Rust/manifests are unchanged. The obsolete runner's125 lock
  guard and archive receipt-key failure stay failed; the current runner uses only the reviewed real lock.
- **Subsequent resolution:**0396 closes the mode/count/range/tree boundary above with723 native,
  restored mutants, fresh AFTER and independent implementation/cost approval. Its commit retains the
  reviewed lock;0394 still requires a fresh baseline before its separate geometry implementation.

### GAP-LARA-BENCH-EXPORT-NAMES — Aggregate benchmark method names exhaust the install limit

- **Status:**0395's two new entries are fixed and runtime-verified; broader capacity remains
  unresolved. Corrected export-section verification and all three BEFORE measurements pass.
- **Observed/owner:** The benchmark-only LARA Wasm's retained baseline exports19963 method-name
  bytes. Two new physical-copy controls raise this to20125 and PocketIC13 rejects install against
  the20000 limit, before any measurement. `canbench-rs-macros`0.7 emits both measurement and trace
  queries and has no name override; `labeled/bench.rs` owns these new entries, not production graph
  state. Official execution-error guidance recommends shorter method names.
- **Evidence:** `vertex-layout-before-slab64.log` and
  `segment-implementation/tiny-leaf-geometry/physical-layout-implementation/export-limit-diagnostics.json`
  in the ADR0097 maintenance evidence bind exact original/current export lists and hashes.
- **Current correction:** Only the two new entries use the existing CDK0.20.1 query-name API:
  `__canbench__vls64` and `__canbench__vlt64` return the same measured fixture/result, with no optional
  trace query. All original exports and saved results stay unchanged; actual corrected sum19997
  is verified from the Wasm section (initial19995 estimate undercounted the12-byte prefix). No production layout/API or generic export framework changes.
- **Impact/next decision:** Two short queries consume34 of37 remaining bytes; broader future
  benchmark additions need a separately reviewed export-set/feature decision rather than repeated
  name compression or deletion of domain coverage. This is not a sandbox failure or permission to
  retry through external terminals.0395 finished its bound BEFORE before production behavior edits;
  `W/physical-layout-implementation/before-{export-verification,benchmark-results}.json` records
  successful install/measurement and unchanged original exports/saved results.

### GAP-TINY-CURSOR-EXTENT — Cursor paging still treats tiny placement as empty

- **Status:** Resolved for slice0381's bounded tiny cursor/selected-read contract; independently
  approved. Disposition **Fix now**. Fixing commit: `fix(lara): preserve tiny cursor extent and
  empty values`, on parent `7651d6e52`; the [implementation report](evidence/adr0097-tiny-cursor-extent.md#review-and-commit-boundary)
  identifies the exact external commit receipt.
- **Observed behavior:** The new `cursor_tiny_pages_keep_used_width` runtime regression confirms
  a four-edge tiny bucket returns `(0, true)` for start0/limit1 instead of `(1, false)`.
  `read_label_bucket_placement_info` deliberately reports zero slab/spill geometry for tiny;
  the cursor incorrectly reconstructed its logical extent from that projection.
- **Expected behavior:** The cursor must cover the descriptor's tombstone-inclusive used prefix;
  placement capacity is not logical extent. See [ADR0096 §5](adr/0096-inline-tiny-mode-for-low-degree-label-buckets.md#5-per-path-rules-dispatch-table).
- **Owner:** `ic-stable-lara`, `labeled/graph/traverse.rs::visit_edges_from_slot_with_inline_property`;
  public caller `DeferredBidirectionalLabeledLaraGraph::visit_forward_edges_from_slot_with_inline_property`.
- **Evidence:** [Cursor extent repair](evidence/adr0097-tiny-cursor-extent.md); regression
  `cursor_tiny_pages_keep_used_width` covers mirrored orientations, both orders, holes, exact
  cursor pairs, zero limits, missing labels, empty buckets and unchanged stable-memory bytes.
  `cursor_slab_pages_keep_retained_spill_and_values` covers independent retained edge/value planes.
- **Necessary read prerequisite (Fix now):** The extent-only candidate reaches the shared
  `visit_edges_at_with_inline_property` path, which interpreted a tiny slot2 tombstone as width65535
  and read outside the value memory. A direct selected-property call reproduces the same failure.
  That reader now dispatches Tiny/Tree/Slab before schema interpretation and reuses the existing
  valueless selected-topology path for tiny; no cursor-specific decoder or fallback was added.
- **Implemented boundary:** The normal bucket cursor resolves the descriptor and consumes the
  existing `bucket_reserved_edge_slots` owner. Duplicate placement arithmetic is removed;
  vertex-owned bypass width, canonical topology decoding, tree/slab value hydration,
  page-end arithmetic and public signatures are unchanged.
- **Impact:** The recorded false-exhaustion and native tombstone/schema failures are repaired.
  The bounded runtime tests and both independent reviews pass; inherited strict lint remains failed.
  No full-suite, canonical-lock, mutation or Graph integration approval is implied.
- **Next decision:** The separate sparse slab work-bound gap below remains open. This repair is
  separate from slice0380's full-label/rank/select contract, not a compatibility path.

### GAP-SPARSE-CURSOR-PREFIX — Sparse slab value pages can rescan the cursor prefix

- **Status:** Open; disposition **Record here**, non-blocking for slice0381's tiny extent repair.
- **Observed behavior:** Static tracing shows that the cursor selects only `[start_slot, end_slot)`,
  but its shared selected-property reader scans `[0, last_requested_slot + 1)` when a property-bearing
  slab bucket has holes, to recover live value ranks. The existing cursor and Graph export comments
  promise no prior-prefix rescan; that work-bound claim is not implemented for this branch.
  No instruction/scaling measurement of these pages ran in slice0381.
- **Expected or needed behavior:** Resolve the cursor's work-bound contract explicitly: bounded
  selected positions are not proof of bounded underlying topology work. Tiny's descriptor-only
  selected reads and native tree property addressing do not use this sparse slab rank scan.
- **Owner:** `ic-stable-lara`, `traverse.rs::visit_edges_at_with_inline_property`'s sparse slab
  live-rank mapping, consumed by `visit_edges_from_slot_with_inline_property`; Graph owns the
  export cursor, not a second physical value-rank decoder.
- **Evidence:** [Cursor repair limitations](evidence/adr0097-tiny-cursor-extent.md#review-and-commit-boundary)
  and [ADR0096 §5](adr/0096-inline-tiny-mode-for-low-degree-label-buckets.md#5-per-path-rules-dispatch-table).
- **Impact:** Later sparse property pages can repeat prior topology reads despite a small page limit.
- **Next decision:** Design and measure the owning LARA cursor/value-rank contract in a separate
  slice, then reconcile the no-prefix-rescan API comments. No cache, checkpoint, Graph workaround
  or weakened runtime assertion is introduced by slice0381.

### GAP-LARA-TEST-CLIPPY — Strict test-target lint diagnostics

- **Status:** Resolved by0393's scoped validation and review; the fixing source is included in the
  validated0386/0393/0395 publication checkpoint. Discovered during the rejected Plan0359
  allocator-operation trial.
- **Observed behavior:** The historical `cargo clippy -p ic-stable-paged-ordered-map -p ic-stable-lara
  --lib --tests --target aarch64-apple-darwin -- -D warnings` reported43 LARA test diagnostics in14
  unchanged files. The later frozen0386 checkpoint had41 across13 exact paths. These are separate
  historical inventories, not interchangeable counts. Categories included unused imports/results,
  redundant casts/borrows, constant-size chunks, single-pattern matches, boolean assertions, doc
  indentation, range indexing and dropping a Copy value.
- **Expected behavior:** The affected Rust test targets must pass strict scoped lint without
  suppressing diagnostics or excluding tests to claim full validation.
- **Owner:** `ic-stable-lara` test modules under `labeled/{graph,bidirectional}` and `lara/vertex.rs`.
- **Evidence:** The [rekey trial](evidence/adr0097-performance-architecture-research.md#13-order-preserving-rekey-rejected)
  preserves the original diagnostics/toolchain/source pins. [0393 validation closure](evidence/adr0097-bucket-segment-publication-preflight.md#validation-closure-0393)
  records actual exit0 for `cargo clippy -p ic-stable-lara --locked --lib --tests -- -D warnings`,
  native708/0 with unchanged allocator34/bucket17 identities, workspace format and independent
  cleanup APPROVE. `validation-closure/baseline.json` binds exact dirty-before paths/bytes; every
  formerly ignored traversal result now asserts exact Continue, and existing test contracts remain.
- **Impact:** Current LARA lib/test lint is clean; production lint or runtime success was not used
  as a substitute. Historical failed commands remain failed and the rejected rekey trial remains
  rejected. No domain, API/layout, fixture or benchmark behavior changed in the cleanup.
- **Next decision:** None for this lint debt. Broader W2,0383 acceptance and separate implementation
  prerequisites remain open; resolving this entry does not approve them.

### GAP-2026-09-22-001 — Property-bearing tree deletion still asserts width zero

- **Status:** Open; live deletion remains paused at the writer/demotion boundary (GAP-003).
  GAP-004's bounded point-state/no-op admission read repair is implemented separately; it
  does not remove the assertion or establish live mutation safety. The bounded tree-compaction
  admission repair is separately approved.
- **Observed behavior:** A real 1024-slot tree produced by `transcribe_and_publish_tree`
  with 33-byte properties panics on `remove_edge_at_slot` in native debug execution:
  `tree mode rejects inline-property buckets (promote is fail-closed)`, left 33/right 0.
  This occurred during fixture setup, before compaction. No release/Wasm deletion result
  was measured.
- **Expected behavior:** Tree producers, deletion admission and the property-plane contract
  must agree. Property-bearing promotion and traversal are implemented; a stale debug-only
  rejection is not a deliberate public error contract. See
  [ADR 0088](adr/0088-tree-csr-mode-for-high-degree-label-buckets.md).
- **Owner:** `graph/tree_write.rs::tree_mode_remove_edge_at_slot` and its removal/demotion
  callers. Promotion is not fail-closed for nonzero properties.
- **Evidence:** Preserved initial fixture and setup-failure log in the
  [tree-compaction evidence](evidence/adr0097-tree-compaction-admission.md#setup-findings-not-compaction-reds).
- **Impact:** Native debug property-tree removal cannot reach its write path. Release-mode
  property preservation, tombstone reuse and demotion safety are not established by this failure.
- **Bounded follow-up:** A real public post-promotion deletion reproduces the assertion.
  With only that assertion temporarily removed, an 8/33-byte Insertion/Unordered owner
  matrix preserves the tested rows, dead property bytes, append/reuse identities and neighbors.
  Public repeated deletion nevertheless returns `Some(tombstone)`, and crossing the demotion
  floor panics after the deletion commit. All diagnostic Rust changes were removed; this is
  not a deletion fix. [Source-pinned evidence](evidence/adr0097-tree-deletion-prerequisites.md).
- **Planned follow-up:** The [ADR0088 demotion/deletion proposal](adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#planned-position-preserving-demotion-and-deletion)
  and [source audit/staged plan](evidence/adr0097-demotion-design.md) require prepared physical
  publication, aligned native selection and complete retirement before enabling live property
  deletion. This is design only, pending review; the assertion is unchanged. Approved point and
  selected read repairs do not prove this boundary. Preserve the accepted compaction fixture;
  its pre-promotion source deletes do not validate tree deletion.

### GAP-2026-09-22-002 — Promoted tree headers do not encode the advertised owner/ordinal metadata

- **Status:** Open; root reachability and byte preservation are tested, not header validation.
- **Observed behavior:** `transcribe_and_publish_tree` constructs a base `BlockHeader` with
  label, owner and ordinal all zero, retaining those values for emitted edge/property blocks.
  A real label-3 root therefore resolves to a header with label zero; the first witness's
  label-equality assertion failed before maintenance. Final witnesses preserve those exact
  producer bytes, distinct root IDs, block kinds/levels/tombstone counts and allocation state.
- **Expected behavior:** [ADR 0088 §4/§8](adr/0088-tree-csr-mode-for-high-degree-label-buckets.md)
  describes owner/ordinal metadata agreement on dereference. Roots remain the sole addressing
  source; metadata is not a second reachability map.
- **Owner:** `graph/promote.rs::transcribe_and_publish_tree`, other tree block producers,
  and tree dereference validation.
- **Evidence:** The same [setup evidence](evidence/adr0097-tree-compaction-admission.md#setup-findings-not-compaction-reds)
  and source-pinned promoter/read code. No forged metadata was inserted to make the witness pass.
- **Impact:** Preservation through compaction does not prove the advertised owner/ordinal
  validation contract. Incorrect header metadata is not evidence that the valid root references
  themselves are aliased or unbacked.
- **Next decision:** Audit all producers and dereference checks, then reconcile implemented
  metadata with the active ADR contract in a separate bounded slice. Do not add another mapping
  or silently claim validation from successful traversal.

### GAP-2026-09-22-003 — Tree demotion does not preserve property and movement contracts

- **Status:** Open; prerequisite to GAP-2026-09-22-001, not repaired in the deletion investigation.
- **Observed behavior:** On a real 1024-position / 33-byte tree, the 512th public deletion
  reaches `T_DEMOTE = 512`. With only the stale deletion assertion removed, demotion panics:
  `live property values must cover degree * w bytes`, 33792 versus 16896. The marker,
  degree (513 → 512) and global count have already changed; extent remains 1024 and the
  observed root bytes remain unchanged. The property collector retains every native slot
  while the edge collector filters tombstones. Separately, source inspection shows that
  successful demotion packs survivors but its removal caller returns no movement events.
  The runtime property panic preempts that later event-parity check.
- **Expected behavior:** Edge/value transcription must select the same live positions;
  survivor ordinals and external identities must agree with the removal/movement contract.
  Rejection must not expose partial publication or release live roots. See
  [ADR 0088](adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#property-bearing-deletion-prerequisite-2026-09-22)
  and [ADR 0097 §7](adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md).
- **Owner:** `graph/tree_write.rs::tree_mode_demote_to_slab` and
  `graph/remove.rs::remove_edge_at_slot_with_move`'s demotion dispatch.
- **Evidence:** [Actual public threshold witness and first-write audit](evidence/adr0097-tree-deletion-prerequisites.md#reachable-demotion-failure).
  Source audit also finds edge-only root avoidance, property-root retirement that assumes
  leaf entries, and fallible work after descriptor publication. Those overlap/depth/failure
  paths were not runtime tested here; the valid witness is distinct from the historical
  synthetic `demote_atomic_on_failure` fixture correction.
- **Impact:** Removing the stale width assertion alone exposes an unsafe public deletion path.
  Filtering property values alone cannot establish movement publication or allocation safety.
- **Planned decision (not implemented or independently approved):** The
  [revised ADR0088 proposal](adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#planned-position-preserving-demotion-and-deletion)
  leaves reclamation policy open after REQUEST CHANGES. Keeping thresholds 512/1024 does not
  approve permanent nonempty `S>1024` retention; a spill-free cap is not the prefix/run envelope.
  The [source audit and resliced plan](evidence/adr0097-demotion-design.md) require allocator
  preparation before whole-unit peer/pin/rewrite publication, aligned native selection and a
  separately proved empty-retirement work bound. No suppression, value-only filter or hidden
  remapping is implemented. Property-tree deletion remains paused; the failure was not rerun.

### GAP-2026-09-22-004 — Tree point admission and matching deletion use slab addressing

- **Status:** Open for matching-removal mutation. The bounded **point-state/liveness READ
  sub-contract** in `70b080ca0` is independently approved through `a80d2e1ac` (base `e9211fcb5`).
  This does not close GAP-001/003, header metadata (002) or landing gates; selected property
  hydration (006) has its own subsequent bounded implementation below.
- **Historical behavior:** The [assertion-only deletion investigation](evidence/adr0097-tree-deletion-prerequisites.md#public-no-op-failure)
  returned `Some(tombstone)` on a repeated public request. The existence helper and point
  reader interpreted tree roots as LEG targets. Bypass point reads also treated the total
  prefix-plus-run extent as a contiguous slab. That evidence remains unchanged.
- **Implemented read boundary:** `graph/traverse.rs::read_edge_state` remains the canonical
  API; its private bucket reader owns mode-aware addressing and state classification.
  `edge_exists_at_slot` consumes it, covering all six forward/reverse removal/property-update
  admissions. The checked bypass projection preserves retained runs; native tree reads reuse
  root resolution without reading property bytes. See the [LARA point-read contract](storage/lara.md#canonical-point-reads).
- **Evidence:** [Six valid-backed red/green witnesses and caller audit](evidence/adr0097-point-state-reads.md)
  cover tiny, slab/run, retained bypass, native tree boundaries, property-bearing tree points,
  populated public no-ops and read-only parallel-edge counterpart ranks. Native visitors remain
  tombstone-inclusive. Scope-specific preservation/error assertions are not general atomicity
  or Graph sidecar integration evidence.
- **Remaining owner:** `graph/remove.rs::remove_edge_matching_skip_leaf_cascade_with_move`
  still groups Tree/Slab for LEG selection and slab deletion; `locate_bucket_edge_for_delete`
  remains a mutation-specific slab/run locator. Neither mutation owner changed. Direct selected
  property reads have a separate value-plane repair (006).
- **Impact:** Correct point liveness is not authorization to enable live property-tree deletion,
  predicate removal, detach or demotion. Their mutation/error/movement contracts remain open.
- **Next decision:** A separately bounded matching-removal design must consume canonical native
  selection and a safe writer, rather than adding caller guards or changing raw visitor semantics.

### GAP-2026-09-22-005 — Obsolete combined maintenance tag discards movement events

- **Status:** Open; pre-production cleanup, explicitly not changed by the deletion slice.
- **Observed behavior:** `CompactVertexEdgeAndValueSpanV1`'s handler ignores both
  `EdgeMoved(_)` and `OverflowRewrite(_)` while requeueing Required compaction in either
  orientation. Repository search finds its enum/tag/codec/key/priority handling and these
  requeues, but no initial enqueue producer. It is not observer-safe.
- **Expected behavior:** Any retained logical compaction path must publish all survivor
  movements through the canonical observer contract; superseded pre-production paths should
  be deleted rather than preserved for compatibility. See
  [ADR 0097 §7](adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md).
- **Owner:** `labeled/bidirectional/deferred.rs::MaintenanceWorkItem`, its codec and
  `maintenance_with_edge_slot_move_observer` handler.
- **Evidence:** [Exact handler/caller audit](evidence/adr0097-tree-deletion-prerequisites.md#obsolete-combined-maintenance-tag).
  No queued-tag runtime witness ran; absence of a producer is a source-search result, not
  proof that the decoder cannot reach the handler.
- **Impact:** The tag cannot be included in the approved movement-publication or observer-safe
  maintenance scope. Existing persisted decoding alone does not justify a compatibility path.
- **Next decision:** Confirm obsolescence and remove its variant/codec/handler together under
  fresh-state policy, or establish a concrete active producer and unify its observer ownership.
  No handler behavior or persistent layout changed in this evidence slice.

### GAP-2026-09-22-006 — Selected tree inline-property hydration used slab live ranks

- **Status:** Resolved in `036649e67` for the bounded selected-read contract; independently
  approved through `f297706ab`. No mutation or general performance/atomicity/space/PocketIC/
  lifecycle/landing approval.
- **Historical behavior:** At `a80d2e1ac`, the direct selected-property path obtains canonical
  topology, then calls `bucket_live_ordinal_at_edge_slot` and slab/run value helpers for trees.
  Genuine 1025-position trees with four transcribed holes panic in the slab collector:
  `slab row must have exactly degree live edges among stored slots` (1025 versus1021 for width8/33;
  1023 versus1021 for width0). The saved backtrace reaches the selected visitor, not setup or bulk
  traversal. The first corrupt-property-root case is preempted by the same rank panic.
- **Expected behavior:** The [selected-read contract](adr/0050-lara-traverse-read-api.md#7-selected-slot-reads-remain-a-first-class-capability)
  uses native property positions for trees and independent live-value ranks for slab/run storage.
- **Owner/fix:** `graph/traverse.rs::visit_edges_at_with_inline_property` dispatches tree reads
  before slab rank/value interpretation, reusing canonical point state and
  `tree_read::read_property_value_at_slot`. Its slot-window and `Traversal` callers share the fix.
  Three unused private point-hydration helpers are deleted rather than acquiring another mode
  dispatch. Native bulk visitors, value/rank mutation helpers and writers are unchanged.
- **Evidence:** [Five read witnesses and source-pinned red/green evidence](evidence/adr0097-selected-tree-properties.md):
  `selected_property_tree_eight_byte_boundaries`, `selected_property_tree_thirty_three_byte_boundaries`,
  `selected_property_tree_zero_width`, `selected_property_tree_corrupt_roots_propagate`, and
  `selected_property_retained_independent_planes`. Four runtime failures plus one already-correct
  retained-plane control become five passes. Native cases preserve all16 backing memories.
  Final corruption checks reject requested bad roots exactly and allow sparse reads of other
  leaves; no new error-after-Break rule is asserted. A wrong native value-position mutation fails.
- **Impact/limits:** Selected live-only output now hydrates native values correctly in the tested
  width8/33/depth1 boundary cases. Raw bulk traversal stays tombstone-inclusive. This is not a
  Graph/mate-cache transaction, deeper-tree/new-width matrix, live mutation or failure-atomicity proof.
  The unchanged scalar control is6214→6218 instructions (+4), not native-tree pricing or a regression waiver.
- **Next decision:** Review this read slice independently. Mutation still requires the bounded
  GAP003 demotion contract: joint live edge/value selection, positions or complete movement
  publication, combined-root reservation and prepare/publish/retire failure boundaries. Do not
  replace that design with a value filter or remove the live-deletion assertion alone.

### GAP-2026-09-22-007 — The older directedness reader bypasses canonical mode dispatch

- **Status:** Open; source-audited only during GAP006's value-helper caller audit. No runtime
  witness or repair in the selected-read slice.
- **Observed source behavior:** `remove.rs::for_each_out_edges_by_directedness_impl` is still
  reachable through `LabeledLaraGraph::for_each_out_edges_by_directedness` and its unchecked
  sibling. It treats a bypass descending extent as one slab, and normal buckets through tiled
  LEG reads or slab iterators/collection plus `attach_edge_inline_property`. It does not dispatch
  tiny/native tree modes. The selected-property fix does not route through this implementation.
- **Expected behavior:** Any retained directedness read surface must share the canonical
  mode-aware topology/value visitor and preserve its label/order/tombstone/Break contract; it
  must not interpret retained runs or tree roots as a slab. See [ADR0050](adr/0050-lara-traverse-read-api.md).
- **Owner:** The two older public wrappers and their shared implementation in `graph/remove.rs`.
  `values.rs::attach_edge_inline_property` also serves matching-removal callers (GAP004); changing
  that shared helper alone would not repair the older reader's topology or order selection.
- **Evidence:** [GAP006 caller audit](evidence/adr0097-selected-tree-properties.md#remaining-boundaries).
  The similarly named deferred forward/reverse adapters instead call
  `visit_out_edges_by_directedness_with_inline_property` and its unchecked variant. Source search
  finds only the two older wrappers calling the older implementation; external library use is
  not excluded. No broad directedness failure is inferred from a selected-read test.
- **Impact:** The bounded selected API and existing canonical adapters do not establish this
  older public surface's tree/tiny/retained-bypass correctness.
- **Next decision:** Separately determine whether to remove the superseded surface under
  fresh-state policy or route it through the existing canonical visitor with valid trait bounds.
  Do not duplicate mode guards, repair matching mutation incidentally or expand the completed
  selected-read validation turn.

### GAP-2026-09-20-007 — The leaf cascade's span rewrite failed for a bucket that owns a spill run

- **Status:** Fixed (F7, `spill-swap`). Found 2026-09-20 by the canbench gate; the four
  gates (`cargo check --lib`, `cargo test --lib`, fmt, clippy) could not see it, and the
  canbench feature's benches were the only witness.
- **Owner:** the labeled leaf cascade (`rebalance_cascade_after_labeled_mutation` ->
  `rewrite_vertex_edge_span` / `materialize_labeled_vertex_edge_plan` /
  `commit_vertex_edge_span_layout`) and the span geometry it sizes.
- **Reproduction:** `cd crates/ic-stable-lara && canbench bench_l_nt_bp`.
  `bench_l_nt_bp_ins_1024` trapped with
  `'Panicked at 'non-tail bypass insert: Store(CollectAllocationOverflow)'`
  (`bench.rs:2905`). The identical command on `main` (`9a64049f4`) ran all three
  `bench_l_nt_bp_ins_*` benches to completion (391.09 K instructions, the +19.32%
  regression the ledger already recorded), so the swap introduced the defect.
- **Cause (measured):** the rewrite's plan counted the bucket's run in `total_live`
  (`bucket_resident_rows` = prefix + run), so `min_required` (65) exceeded the span the
  vertex already had (64), the plan requested 144 slots
  (`next_vertex_edge_span_allocation`), and `finish_span_resolution` reported
  `resolve_labeled_edge_base: no placement for a required span of 144 slots`; with the run
  excluded from the plan but the position calculator's `degree` floor left in place it
  became `span_slots=64 < effective_live=65`. `main` made room for the fold by growing the
  vertex span first - the step ADR 0097 removes.
- **Fix (decision (b): a relocation keeps the run, positions do not change, only an
  explicit compaction folds):** the rewrite plan counts the slab prefix; the plan's
  spanless-vertex shortcut honours `preferred_extra` (a caller asking for real room must
  get a span); the commit's resident scratch keeps the run for a keeping relocation
  (otherwise the published descriptor lost it and the rows became unreachable -
  `slab scan ended before yielding all logical edges`); the position calculator's slab
  `degree` floor goes (the materialized width is the SSOT: a relocation's prefix, a
  compaction's packed live rows); and the wide-edge carve-out, which cannot promote, now
  goes through the explicit compaction instead of a span-growing rewrite, extracted as
  `compact_run_into_prefix_with_growth_retry` and, at that revision, shared with promotion's
  Phase 0a. `15ab44efb` subsequently removes promotion's fold; the wide-edge carve-out remains.
- **Witness:** `labeled::graph::spill_swap_test::a_dense_leaf_rewrite_keeps_the_run_and_its_ordinals`
  (the bench's fixture). It fails before the fix (`insert 1000000:
  Store(CollectAllocationOverflow)`) and passes after, with the run and its ordinals intact
  and the descriptor still pointing at it.
- **Cost, attributed and closed (F8).**
  `cd crates/ic-stable-lara && canbench bench_l_nt_bp` (same command, same fixture) measures

  | bench | `main` 9a64049f4 | `spill-swap` 52b2c7796 | ratio |
  | --- | --- | --- | --- |
  | `bench_l_nt_bp_ins_256` | 390,604 | 1,164,747 | 2.98x |
  | `bench_l_nt_bp_ins_1024` | 391,094 | 1,163,272 | 2.97x |
  | `bench_l_nt_bp_ins_4096` | 391,094 | 1,163,272 | 2.97x |

  Flat across all three fixture densities on *both* sides, so this was a regression of the
  swap, not a cost of the fixture shape. The scope call counts named the mechanism: `main`
  runs the leaf cascade **once** for the 16 inserts (`labeled_rebalance_leaf_cascade`
  1 call / 194,089 instructions, of which `labeled_rebalance_edge_log_vertex` - the fold -
  is 185,893), because that single run re-lays the leaf and drops its density below the
  trigger (`LEAF_VERTEX_EDGE_SEGMENT_DENSITY = 1.0`). The swap head runs it **16 times**
  (927,185 instructions, of which `labeled_rewrite_vertex_edge_span` is 16 calls /
  810,149): a bucket whose rows live in its run keeps its span in place, so the rewrite
  changes nothing about the leaf's geometry, the density stays at 1.0, and every insert
  re-enters the cascade and repeats the same rewrite. The slack placement search is only
  `labeled_rewrite_read_and_plan` (16 calls / 60,155, ~5% of the total); the cost was the
  repetition.

  **Fix (F8):** the rule was already in the tree - db04af585: speculative slack must not
  force allocation when the content already fits, and may use extra space only when it is
  currently placeable. The cascade's rewrite of the source vertex is a *forced-slack*
  rewrite, so it is skipped when the vertex is not span-resident (any bucket owning a run):
  there is no span content to grow, the run holds those rows, and the rewrite could only
  re-publish the same layout. The rest of the cascade still runs. Measured with the same
  command and fixture: 364,070 / 363,906 / 363,906 for `ins_256` / `ins_1024` / `ins_4096`
  against `main`'s 390,604 / 391,094 / 391,094 - the cascade's 16 calls now cost 129,776 in
  total and no `labeled_rewrite_vertex_edge_span` scope appears. Witness:
  `a_run_backed_vertex_is_not_force_rewritten_by_the_cascade` (15 forced rewrites before,
  0 after).

### Historical landing evidence — selected pre/post-5i rows

Main values below are rounded console totals from `/tmp/bench-main-full.log`, not exact
persisted measurements. The earlier parser dropped the B suffix; those entries are corrected
below using the recovered 189-name comparison. Pre/post columns describe 5i, not current HEAD.
The earlier claims that this was a complete >5% list and that exactly one row changed were
incorrect: candidate workloads also changed substantially. The recovered current comparison
has 38 differences above +5% against rounded main; see GAP-009 for the scoped diagnosis.

| bench | main (measured) | pre-5i | post-5i |
| --- | --- | --- | --- |
| `offset_workload_candidate_bursty` | 1,390,000,000 | 1,465,455,879 | 491,406,745 |
| `offset_workload_candidate_uniform` | 1,400,000,000 | 1,433,556,965 | 471,147,924 |
| `offset_workload_candidate_churn_light` | 1,250,000,000 | 1,252,356,452 | 401,477,074 |
| `tcsr_65536_insert_grow` | 1,120,000,000 | 1,118,273,942 | 1,118,273,942 |
| `tree_churn_bursty_reuse` | 1,040,000,000 | 1,041,741,982 | 1,041,741,982 |
| `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384` | 15,540,000,000 | 15,542,810,613 | 43,911,532 |
| `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_65536` | 247,360,000,000 | 247,370,242,366 | 247,370,340,681 |
| `bench_t_vip_descending` | 27,010 | 197,860 | 197,860 |
| `bench_t_vip` | 28,060 | 197,851 | 197,851 |
| `bounded_inline_promote_full` | 216,850 | 1,132,525 | 1,132,525 |
| `bench_l_mix_hub_scn_33x50` | 350,320 | 1,710,929 | 1,710,929 |
| `bench_l_mix_hub_asc_33x50` | 452,960 | 2,011,687 | 2,011,687 |
| `bounded_inline_64_full` | 18,970 | 80,251 | 80,251 |
| `bench_t_v_in_hyb_ip` | 74,470 | 292,185 | 292,185 |
| `bench_t_v_pf_then_sel` | 152,750 | 512,905 | 512,905 |
| `bench_t_v_hyb` | 81,010 | 241,373 | 241,373 |
| `bench_r_ed_st_d_oi_lb_128` | 27,090 | 80,452 | 80,452 |
| `bench_l_s2_h_scn_d_1024` | 17,720,000 | 51,932,493 | 51,932,493 |
| `bench_l_s2_h_pt_d_1024` | 17,770,000 | 51,985,176 | 51,985,176 |
| `bench_t_v_in_dense` | 88,130 | 254,474 | 254,474 |
| `bench_r_ed_st_oi_lb_128` | 35,380 | 88,746 | 88,746 |
| `bench_l_s2_h_del_1024` | 127,160,000 | 266,537,637 | 193,429,761 |
| `bench_t_vip_filtered_reference` | 154,220 | 318,265 | 318,265 |
| `bench_t_v_w_ip` | 155,530 | 320,305 | 320,305 |
| `bounded_topology_64_full` | 32,950 | 63,238 | 63,238 |
| `bench_t_lvp_same` | 363,100 | 528,866 | 528,866 |
| `offset_workload_baseline_bursty` | 304,330,000 | 417,022,107 | 417,022,107 |
| `offset_workload_baseline_uniform` | 301,930,000 | 376,405,141 | 376,405,141 |
| `bench_t_v_at_replay` | 4,989 | 6,107 | 6,107 |
| `bench_l_ins_fresh_256` | 33,150,000 | 39,224,402 | 39,224,402 |
| `bench_l_ip_frag_comp` | 38,220,000 | 43,470,216 | 43,470,216 |
| `bench_l_ip_frag_s4_ctl` | 38,210,000 | 43,458,762 | 43,458,762 |
| `bench_l_df_ip_frag_enq_6` | 76,550,000 | 87,051,135 | 87,051,135 |
| `bench_l_ip_frag_s6` | 38,280,000 | 43,527,769 | 43,527,769 |
| `bench_l_df_ip_frag_mnt_6` | 76,660,000 | 87,162,578 | 87,162,578 |
| `bench_l_ip_pstats_16` | 41,160,000 | 46,569,383 | 46,569,383 |
| `bench_l_ip_pstats_64` | 59,400,000 | 67,018,574 | 67,018,574 |
| `bench_l_ins_ap_hub_1024` | 10,320 | 11,536 | 11,536 |
| `bench_l_ip_exg_256` | 44,740,000 | 49,900,227 | 49,900,227 |
| `bench_l_ip_pstats_256` | 285,060,000 | 312,686,647 | 312,686,647 |
| `bench_l_s2_h_del_h_1024` | 72,740,000 | 77,908,039 | 77,909,616 |
| `bench_r_ed_st_ls_128` | 346,750 | 369,658 | 369,658 |
| `bench_l_nt_bp_ins_256` | 390,600 | 365,548 | 365,548 |
| `bench_l_nt_bp_ins_1024` | 391,090 | 365,548 | 365,548 |
| `bench_l_nt_bp_ins_4096` | 391,090 | 365,497 | 365,497 |

Headline (better than main, unchanged by 5i): the three rows above.

Space audit (GAP-006 shape, segment16/uniform256, 5000 vertices x 1 edge, 5000 tiny buckets,
cover 0): committed pages **66 (main) -> 56 (36-byte interim row) -> 41 (34-byte final row)**;
descriptor 25,000 x 34 B = 850,000 B; edge spill 1 page, values spill 1 page.

### Historical finalize experiment — notification acceptance withdrawn

The measurements below belong to the earlier experiment, not a valid movement-publication
contract. Its `Finished` rewrite folded and repacked rows without delivering their changed
ordinals. The former “fits by construction” justification did not establish notification or
relocation safety. `15ceaabf1` removes that path: completion packs only captured physical regions,
retaining run/prefix ordinals when the non-forcing fold declines. [Corrected evidence and owner/observer witnesses](evidence/adr0097-compaction-movement-publication.md).
The instruction/space figures below remain historical observations, not revalidated landing evidence.

| bench | main (measured) | pre-fix | post-fix |
| --- | --- | --- | --- |
| `bench_l_s2_h_del_1024` | 127,160,000 | 193,429,761 | 193,655,245 |
| `bench_l_s2_h_pt_d_1024` | 17,770,000 | 51,985,176 | 51,985,176 |
| `bench_l_s2_h_scn_d_1024` | 17,720,000 | 51,932,493 | 51,932,493 |
| `bench_l_s2_det_hub_st_1024` | 45,845,565 | 57,849,414 | 45,848,699 |
| `bench_l_s2_det_sat_1024` | 465,196 | 937,953 | 465,286 |
| `bench_l_s2_h_del_h_1024` | 72,740,000 | 77,909,616 | 77,908,562 |
| `offset_workload_baseline_bursty` | 304,330,000 | 417,022,107 | 417,022,107 |
| `offset_workload_baseline_uniform` | 301,930,000 | 376,405,141 | 376,405,141 |
| `bench_l_nt_bp_ins_256` | 390,600 | 365,548 | 365,548 |
| `bench_l_nt_bp_ins_1024` | 391,090 | 365,548 | 365,548 |
| `bench_l_nt_bp_ins_4096` | 391,090 | 365,497 | 365,497 |

Space audit (GAP-006 shape, segment16/uniform256, 5000 vertices x 1 edge, 5000 tiny buckets,
cover 0): committed pages **66 (main) -> 56 (36-byte interim row) -> 41 (34-byte final row)**;
descriptor 25,000 x 34 B = 850,000 B; edge spill 1 page, values spill 1 page.

### GAP-2026-09-20-009 — Experimental: redundant header reads and scalar spill payload reads

**Status: tail four-byte public capacity promotion and run-retaining leaf relocation have bounded witnesses; header read batching improves mixed-hub measurements. Broader caller/atomicity, performance and landing gates remain open.** Diagnostic source
revision `c5015581d4f709902ce9240a704dbbfe08cbeced`. The measurements below describe the
reviewed experiment, not the subsequent production implementation.
The previous unavoidable-capacity-cost verdict is withdrawn. ADR 0097's contiguous runs,
lazy allocation, used bounds and stable ordinals do not require scalar reads. The previous
4-byte/segment16/uniform256 probe scanned after deletes and did not reproduce these benchmarks.
Its aggregate read counts neither identified span work nor established an unavoidable cost.
Main's prefix also contains tombstones; the earlier claim that main only visits live rows was wrong.

The guard removal in `0b6673a0d` affected the hybrid-values helper, which is not the path of
these scan benchmarks. That diagnosis did not validate it. The later actual-consumer witnesses
below reproduce contract failures and `8a6bafd1b` restores checked admission while unifying the
ordered emitter. It is not credited as a width-zero scan fix. This entry replaces both
contradictory GAP-009 entries.

#### Actual paths and fixture parity

`bench_l_s2_h_pt_d_1024` and `bench_l_s2_h_scn_d_1024` each seed 1024 live rows, then perform
51 full descending callback scans, with no preceding deletes. They call
`for_each_edges_for_label_ordered -> visit_slab_bucket_span_edges -> desc_out_edges_iter ->
spill_replay_entries -> spill_edge_at`. Replay retains decoded edges, not tags requiring a
second payload read. Matching removal scans the prefix then calls `spill_edge_at` in an
ordinal loop restarted for each target `0,2,...1022`.

The source's `BenchEdge::BYTES` is **10**, not 4; it encodes the target in four bytes and pads
six bytes. The native probe copies that edge implementation and uses `LabeledLaraGraph::new`,
`InitialCapacities::uniform(1 << 20)`, label 2/default label 1 and the actual insertion policy.
The resulting shape is **1 prefix row + 1023 spill rows**. Point and full-scan workloads use
separate fresh graphs. Delete measurement separates matching removal, the existing
`SlackMayBeDropped` compaction loop, and the survivor scan.

Benchmark source and persisted YAML SHA256 remained unchanged throughout A/B:

- `src/labeled/bench.rs`: `bb12331194546dbd91ae1ef5e0b5ba0d3fc141ffd93ba8b71ed956b4b76acbc7`
- `canbench_results.yml`: `eccea4b7f18e650d053499cf20bbaaa0021aebeb7d5d134c4083011de76cafc1`

Main-vs-branch scan fixtures are unchanged. Delete **totals are not strictly same-operation**:
`7037b1680` changed the compaction policy. Compare the same matching-remove loop separately;
current-vs-experimental totals use identical benchmark source and maintenance semantics.

#### Controlled experiments and exact instruction counts

H1 adds a checked read inside `SpillRunStore`, validating used bounds once, and switches the
scalar wrapper to it. Payload reads remain scalar. H2-scan changes only replay payload reads
to chunks of at most 32 rows. H2-both additionally batches matching-removal reads into at most
32 rows, decoding lazily in the original ordinal loop. No descriptor, policy, ordinal,
tombstone or persistent layout changes; no public unchecked read API. Class slack is rejected.

| Benchmark / scope | main rounded console | current exact | H1 exact | H2-scan exact | H2-both exact |
| --- | ---: | ---: | ---: | ---: | ---: |
| `bench_l_s2_h_pt_d_1024` | 17.77 M | 51,985,176 | 39,359,361 | 17,079,705 | 17,079,705 |
| `bench_l_s2_h_scn_d_1024` | 17.72 M | 51,932,493 | 39,306,678 | 17,027,022 | 17,027,022 |
| `bench_l_s2_h_del_1024` | 127.16 M | 193,655,245 | 129,582,001 | 129,582,001 | 21,378,000 |
| `bench_l_s2_h_del_1024::labeled_remove_edge_skip_leaf` | 60.57 M | 189,731,407 | 126,155,342 | 126,155,342 | 17,951,339 |

The exact committed main artifact at `9a64049f4` instead contains totals 17,768,910 /
17,716,329 / 127,044,563. These are not the later fresh console measurements. Fresh main
exact CSV is unavailable; do not manufacture precision from K/M/B output.

#### Native counters: Memory reads, not IC syscalls

The wrapper classifies the seeded run's four-byte used header by address, spill payload by
its used address interval, prefix memory separately, and all other memories by role. Counters
are reset between phases. Counts below are `read calls / bytes`. The raw logs retain all roles.

| Phase / read kind | current | H1 | H2-both |
| --- | ---: | ---: | ---: |
| Point **or** full scan, spill header | 104,397 / 417,588 | 52,224 / 208,896 | 1,683 / 6,732 |
| Point **or** full scan, spill payload | 52,173 / 521,730 | 52,173 / 521,730 | 1,632 / 521,730 |
| Point **or** full scan, prefix | 51 / 510 | 51 / 510 | 51 / 510 |
| Matching remove, spill header | 524,797 / 2,099,188 | 263,165 / 1,052,660 | 9,949 / 39,796 |
| Matching remove, spill payload | 261,632 / 2,616,320 | 261,632 / 2,616,320 | 8,416 / 2,692,970 |
| Matching remove, prefix | 512 / 5,120 | 512 / 5,120 | 512 / 5,120 |

Both scan workloads decode 52,224 live / 0 dead rows and invoke 52,224 callbacks in every
variant. Matching removal decodes 131,328 live / 130,816 dead rows and invokes the predicate
131,328 times in every variant. The prefix contributes 512 decoded rows (one live, 511 dead);
subtracting these from total decodes gives spill 131,327 live / 130,305 dead. Batching may
fetch a suffix beyond the match, but does not decode it or invoke extra predicates. It reads
76,650 additional payload bytes across 512 deletes while eliminating most scalar calls.

The H1 reduction is exactly one header read per visited spill row: 52,173 per scan workload
and 261,632 during matching removal. Baseline scanning spends two header reads per spill row
plus one per replay; baseline remove also has loop/write bookkeeping. The earlier static
519,180 model used the different five-prefix-row fixture and must not be compared directly
to this one-prefix-row, 10-byte fixture.

Compaction is recorded separately: current header 4,099 / 16,396 and spill payload
2,046 / 20,460; H1 and both H2 variants header 2,053 / 8,212 with payload unchanged.
The survivor scan has no spill reads, one prefix read / 5,120 bytes, and 512 live decodes
in every variant. Replay batching alone leaves delete instructions unchanged, confirming
that the expensive matching path is separate from replay.

**Ranking:** H2 read batching delivers the largest measured reduction; H1 duplicate-header
removal independently delivers a substantial reduction. H3 repeated traversal/tombstone
visits exist but remain identical across all experiments while performance recovers; they
are not a necessary change for this slice. No H3 traversal-policy change was tested or is
recommended. The experiment rejects the capacity-waiver premise, not the existence of all
other regressions.

#### Recommendation and validation limits

Smallest fix recommendation: one checked bounded-read operation owned by `SpillRunStore`,
used by edge replay and matching removal to read contiguous chunks without changing callback
order or used bounds. Keep write/class-capacity APIs distinct. Production implementation
needs focused bounds, partial-chunk, early-match and tombstone/order tests; review buffer
sizing across edge widths. The diagnostic patch is not a production-ready API proposal.

Experimental library suite: **625 passed / 0 failed** (623 existing tests plus two diagnostic
tests). The bounds witness rejects class slack, crossing the used end and `u32::MAX`, leaving
the output untouched on rejection. Probe/test patches are retained outside production source.
Experimental source was removed after measurement. Those measurements are not final landing
validation and were not persisted.

The production implementation adds `SpillRunStore::read_rows` (one header snapshot,
`MAX_ROWS` and used-range validation, empty ranges valid at/before used end, no output changes
on rejection). Scalar reads share that invariant. `EdgeStore::spill_edges_iter` uses checked
buffer sizing and the existing slab chunk count, decodes lazily, and serves replay plus both
labeled and bypass matching. Writes remain class-capacity checked and unchanged. Point
lookups, compaction's direct reads, batch-write point access, values-store callers, and the
then-unvalidated hybrid-values change were deliberately outside that batching slice.

Owning witnesses: `bounded_reads_reject_slack_overflow_and_corrupt_lengths_without_output_changes`,
`spill_replay_reads_chunks_not_scalar_rows`, `spill_replay_reads_chunks_for_ten_byte_rows`,
`spill_matching_preserves_ordinals_and_stops_predicates_at_match`, and
`spill_matching_attaches_sidecars_before_predicates_and_preserves_survivors`. The replay
read-count witness failed on the old scalar implementation (202 reads versus 7 for 67 rows)
and passes on the production implementation. Coverage includes 4/10-byte rows, chunk boundaries,
partial tails, tombstone ordinals, both scan directions, labeled matching and sidecars.
The initial claim of bypass spill-matching coverage was incorrect: bypass mode alone did not
prove a spill existed. The strengthened fixture now requires a mintable non-sentinel run
with more than one chunk of used rows; its canonical bucket-to-bypass conversion exposes
the failure described below before all requested matching cases can run.

Historical validation at `e5f374594`: library suite **628 passed / 0 failed**, with the
inadequate bypass fixture. Its 90-second focused canbench timeout was incomplete compilation,
not a measured regression.

Correction `e9313062d` makes chunk loading depend on the requested index's chunk. Fresh
`nth(1)`, `next()` followed by `nth(chunk)`, and `last()` identity tests cover 4/10-byte rows
and a partial last chunk. These three tests also **pass before the correction** on installed
Rust `1.100.0-nightly (574ff7d98)`: its `Map` has no `nth` override and visits skipped mapped
items. No reproduced red signal is claimed for that report. The original 202-versus-7 replay
read-count witness remains valid.

Before `bbe4d04e5`, the stronger graph fixture failed against both `e5f374594` and the correction. It seeds a
real label bucket, converts with `enable_default_edge_bypass`, verifies the run, tombstones
slots 31/32/66, then checks exact slot/target order. Ascending traversal returns the five real
prefix rows followed by zero-filled span rows instead of the run's live rows. The owning
path is `out_edges_iter -> on_slab_edges_with_layout`: bypass `stored_degree` includes run
rows, while prefix sizing uses the slab window. This exposes a separate prefix-extent
contract problem, now fixed in `bbe4d04e5` with the strengthened fixture committed. The old
failing fixture and output were preserved at
`/tmp/spill-corrections/bypass-coverage-blocker.patch`. The first-spill-row match case and
sidecar spill preconditions are included; sidecar checks pass.

Historical validation before the extent fix: **630 passed / 1 failed**. Scoped formatting
and `cargo clippy -p ic-stable-lara --lib -- -D warnings` pass. Production focused runs completed
without persistence; [exact totals and matching scope](evidence/adr0097-production-read-granularity.csv):

| Benchmark / scope | production exact | main rounded console |
| --- | ---: | ---: |
| `bench_l_s2_h_pt_d_1024` | 23,176,500 | 17.77 M |
| `bench_l_s2_h_scn_d_1024` | 23,123,817 | 17.72 M |
| `bench_l_s2_h_del_1024` | 50,004,983 | 127.16 M |
| `bench_l_s2_h_del_1024::labeled_remove_edge_skip_leaf` | 46,647,860 | 60.57 M |

The scans remain about 30% above rounded main. Main delete totals have different compaction
semantics; compare its same matching loop separately. These are production results, not the
lower diagnostic H2 numbers. Commands from `crates/ic-stable-lara`: `timeout 300 canbench
bench_l_s2_h_pt_d_1024 --csv` (54 seconds), then `timeout 120 canbench
bench_l_s2_h_scn_d_1024 --csv` (5 seconds) and `timeout 120 canbench
bench_l_s2_h_del_1024 --csv` (4 seconds). All exited 0. Logs, exact CSVs, standard-library
source evidence and red/green results are in `/tmp/spill-corrections/`.

#### Completed correctness and production follow-up

`bbe4d04e5` fixes the shared initialized-extent decision. `CsrVertex::initialized_slab_slots`
projects total width minus run rows for core/bypass rows; `LabelBucket` projects its stored
prefix directly. `EdgeStore::initialized_slab_slots` validates the run and checked extent,
replacing capacity-window inference in both scans and slot-location consumers. Bypass
existence/matching/removal use that same owner; the separate saturating helper is deleted.
The graph-kernel wrapper inherits the core/bypass projection through its delegated width.
No storage layout, row movement or compaction policy changes. The committed real conversion
fixture now verifies 4/10-byte rows, mintable runs crossing a chunk, pre/post-tombstone scans,
first prefix/first spill/middle/last/miss cases, predicate order, exact surviving slots and
empty movements. The inherited conversion witness now explicitly requires a nonempty run.

Performance follow-up measured one change at a time (exact instructions):

| Variant | point | scan | delete total | matching scope |
| --- | ---: | ---: | ---: | ---: |
| Extent fix only | 23,177,877 | 23,125,194 | 50,004,983 | 46,647,860 |
| Remove speculative loaded-chunk state | 22,441,437 | 22,388,754 | 46,310,515 | 42,953,392 |
| Additionally collect replay directly in checked chunks | 17,178,033 | 17,125,350 | 46,310,515 | 42,953,392 |
| Final shared checked buffer setup, `d654992be` | 17,121,984 | 17,069,301 | 46,311,545 | 42,954,414 |
| Diagnostic direct labeled matching, not landed | not measured | not measured | 25,245,782 | 21,886,622 |

The matching-only experiment retains the same safety checks and callback order; native
header/payload/byte/decode/predicate counts are identical to final production matching.
It attributes a substantial remaining prototype gap to iteration execution overhead, not
capacity. Its diagnostic-only API/loop patch was reverted and preserved, not published as a
production implementation. It remains an optimization opportunity, not a correctness gate
or an unavoidable representation cost. Final replay and streaming iteration share checked
buffer setup; full replay avoids per-row fallible iterator overhead. The identity tests remain;
the withdrawn nth/last claim is not used as a reason to retain loaded-chunk state.

Final native counters: each no-delete 51-scan workload has 1,734 header reads / 6,936 bytes,
1,632 spill payload reads / 521,730 bytes, 52,224 live decodes/callbacks and zero dead decodes.
Matching removal has 9,949 header reads / 39,796 bytes, 8,416 payload reads / 2,692,970 bytes,
131,328 live / 130,816 dead decodes and 131,328 predicate visits. Prefix and compaction phases
remain separately recorded in `/tmp/spill-perf/native-final.log`.

Validation: **631 passed / 0 failed**, library clippy `-D warnings` and touched-file formatting
checks passed. Final focused point/scan/delete commands all passed, without persistence.
The subsequent **unfiltered `canbench --csv --persist` completed all 189 benchmarks** in
176 seconds, exit 0, and reproduces the final named totals above. All 189 totals were compared
name-by-name with recovered rounded main: **22 remain above +5%**, down from 38 at `c501`;
none of the 189 totals regress by more than 5% against the `c501` exact totals. This is not
broader landing approval. Main delete total still has different compaction semantics; its
matching scope is the comparable loop. Final scan totals are below rounded main 17.77 M /
17.72 M. The final headline rows `bench_l_nt_bp_ins_256/1024/4096` were actually remeasured at
364,463 / 364,463 / 364,412, below rounded main 390.60 K / 391.09 K / 391.09 K. Capacity
**66 -> 41 pages remains historical, not remeasured in this slice**.

Raw commands, patches, native probes/logs, focused/full CSVs and all-name comparison are in
`/tmp/spill-extent/` and `/tmp/spill-perf/`. Durable final name-by-name evidence is
[evidence/adr0097-final-read-comparison.csv](evidence/adr0097-final-read-comparison.csv).
The benchmark source hash is unchanged; the YAML changed only through the complete final run.
Main is untouched and unmerged.

For provenance, the original diagnostic used `c501` headline totals 365,548 / 365,548 /
365,497 and the recovered comparison then had 38 differences above +5%. The completed
follow-up numbers above supersede those branch measurements; they do not replace the historical
capacity audit or authorize landing.

Durable instruction evidence: [ADR 0097 read-granularity CSV](evidence/adr0097-read-granularity.csv)
records exact current/H1/H2 totals and matching scopes at the diagnostic source revision.
Reproduction artifacts: `/tmp/remove-scan-diagnostic/` contains the installer, native probe,
bounds test, H1/H2 patches, raw native logs and exact canbench CSVs, original rounded-main log,
reviewer comparison, checksums and `README.md` with commands. `main` was not edited or rerun.

#### Retained-run append and mixed-hub follow-up

`c632f5fc5` fixes the runtime-reproduced tail-bypass append failure at the common core insertion
decision, retaining prefix/ordinals and typed exhaustion. `84ac9203f` avoids zero-width property
helper work: mixed-hub ascending **1,017,187 -> 904,624**, descending unchanged **713,437**.
The separate topology-delegation experiment was rejected. **634/0** library tests, scoped
formatting and strict library clippy passed. Mixed-hub differences remain approximately 2x
rounded main; neither their remaining cause nor the distinct values-batch regressions is closed.
The earlier complete YAML is preserved; no filtered results were persisted. Runtime red/green,
source parity, counter evidence, exact variants and the source-only all-label bypass-reader
follow-up are recorded in [append/mixed-hub evidence](evidence/adr0097-append-mixed-hub.md).

#### All-label, transfer and checked-growth boundary follow-up

`f142a45de` corrects runtime-red 4/10-byte all-label descending bypass reads by delegating to
`visit_edges`. It also fixes the reproduced reverse-conversion `SpillRunOnNormalRow` panic,
transferring prefix/run ownership without compaction, including zero-live retained runs.
`4451efbd3` fixes the kernel wrapper's fallible-growth forwarding. LARA **637/0**, kernel
**202/0**, scoped formatting and strict library clippy passed.

**Capacity integration remains a prerequisite, not accepted rejection semantics.** Actual public
`insert_forward_out_edge` returns `Forward(Store(SpillNeedsPromotion))` at MAX_ROWS for 4/10-byte
rows with and without tombstones. At `4451efbd3`, after safe conversion, directly invoking the existing 4-byte
promoter with prefix 5, used 1024 and 16 tombstones returns `AllocSpaceCapReached`
(`current_alloc_space=1018, cap=1024, mode=Slab`) after freeing the run and moving target 16 from ordinal 16 to 5. With eight tombstones it returns
success but emits five prefix tombstone sentinels and renumbers survivors. The owner is
`promote_bucket_to_tree_mode_impl`'s Phase 0a fold before admission/transcription. Promotion must
preserve ordinals and preflight admission before mutation; do not blindly wire the bypass signal
there, lower thresholds, or force compaction. The wide-edge carve-out is distinct from the
four-byte tree path. Arena exhaustion and other outer-write atomicity remain unvalidated.

Focused correctness-source checks at `4451efbd3`: mixed ascending **904,471**, descending
**713,437**, non-tail bypass insertion 256 **363,627**; no new cause is assigned to small deltas.
The existing main-reference worktree is dirty, so the required main-side counters and subsequent
optimization experiment are blocked. No reference files or full artifacts were changed.
[Detailed evidence](evidence/adr0097-append-mixed-hub.md#capacity-integration-blocked-not-relabeled-as-success)
records exact public states, red/green evidence, caller trace and the remaining decision.

#### Ordinal-preserving promotion prerequisite

`15ab44efb` removes promotion's destructive source folds. Admission uses original resident width;
native edge/property trees preserve surviving ordinals and high-water extent. The identical final
seven-test fixture set records **1 passed / 6 failed** against the old promoter and **7/0** in the
new library run (**644/0** overall). A real LTB grow failure after two target blocks verifies exact
typed rejection, unchanged source descriptors/rows/counts, and return of both blocks and the full
root allocation. Native tree visitors intentionally emit tombstone rows; tests assert their exact
positions and leaf-header counts rather than filtering them out. These replace the earlier promoter
counterexamples, not the public caller-integration requirement.

At `15ab44efb`, public bypass conversion/promotion/retry remained unimplemented as one failure boundary. Separately,
a cascade-path fixture exposed an active prefix `[256,261)` already present in free span `[256,267)`
before promotion. Its next root allocation reused that source prefix. The actual deferred forward
fixture does not reproduce this ownership defect. No allocator workaround or automatic promotion
routing was added. [Evidence and exact validation](evidence/adr0097-promotion-prerequisite.md).

#### Bypass footprint and public class-boundary corrections

`1846cb7d3` fixes the first corrupting cascade append (**67**, both 4/10-byte rows): leaf planning
and snapshots skipped bypass vertices, then retired their still-referenced prefixes. The common
leaf commit now moves captured bypass prefixes and republishes their origin before footprint
retirement, without folding their runs. Durable regressions assert every append, run ownership,
free-span disjointness and subsequent allocation/conversion/promotion. The exact original diagnostic
is preserved under `design/evidence/sources/adr0097-cascade-fixture.rs`.

`4deef5a2c` replaces periodic sidecars with injective target identities and checks the neighboring
label. A property-leaf alias passes the old fixture but fails the strengthened fixture. Deletion
is explicitly asserted to fold away the values run; retained values-run-plus-holes coverage is not
claimed.

`6c5822a79` integrates tail, same-label four-byte class exhaustion through the actual
`insert_forward_out_edge` call. Native tree plus pending append and bucket capacity are prepared
while the bypass remains canonical; only the final tree is published. Success with 0/8/16 holes and
a real target-allocation failure with byte-exact source preservation, allocation cleanup and retry
are covered. Direct store-level typed rejection witnesses remain. Final library **650/0**, strict
library clippy and scoped formatting passed. Wide edges, arena exhaustion, other caller shapes,
normal bucket-mode relocation folding, and broader first/second-write atomicity remained open at that revision.

A fresh disposable `git archive` of `9a64049f4` and the branch ran the same 33×50 native probe,
sequentially sharing the normal target. Main stores most rows in prefixes; the branch retains
33 runs. Both decode/call back exactly 1,650 live rows. The branch performs **492 more reads** per
traversal, with slightly fewer total bytes: bucket reads increase by 330 and the log/run memory
reads increase from 6 to 198, offset by 30 fewer slab reads. These are measured differences,
not individual IC instruction-cost attribution. Focused canbench at `6c5822a79`: **896,221 ascending**,
**713,437 descending**, **106,087 bypass conversion**; no full artifacts changed. Mixed-hub and
broader performance work remains open. [Detailed evidence](evidence/adr0097-capacity-integration.md).

#### Normal relocation, late reservation failure, and batched headers

`40faf5f59` retains normal runs in both leaf materialization paths. The seven placement failures
among the earlier nine failures traced to physical-width admission followed by logical-degree
cursor advancement; placement now reuses the admitted widths. The other two exposed prefix-only
promotion eligibility, corrected to use logical resident extent at the unchanged threshold. The
logical resident helper and reservation/slack policy were not redefined. Strengthened fixtures
(`0017ac655`, `6b144715d`) cover nonempty normal prefixes, holes in prefix and run, independent
8/33-byte sidecars, mixed normal/bypass owners, disjoint and genuinely overlapping moves, and free
span ownership. A public bucket-reservation failure after both target leaves are prepared proves
cleanup, byte-exact prefixes, unchanged source/counts, a cached peer and successful retry; the
LTB-growth witness remains. Final library **653/0**, strict library clippy and scoped formatting.

`c2088b8e0` batches `EdgeSlabStore::read_header` into one 48-byte read, retaining current-field and
layout validation. The one-factor comparison against `40faf5f59` measures **896,221 → 790,432**
ascending and **713,437 → 557,118** descending. Bucket read calls fall **737 → 134** and
**1,089 → 198**, with unchanged bytes, other-role reads, exact rows, decodes and callbacks. These
are branch measurements, not a new main comparison or an instruction cost assigned per read.
No full benchmark artifacts were persisted. Wide/arena/other-caller and broader atomicity gates,
remaining regressions, space/PocketIC and final all-name validation remain open.
[Exact witnesses, CSVs, diagnostics and limitations](evidence/adr0097-normal-relocation-header-reads.md).

#### Pinned full survey and actual hybrid-value consumers

The unfiltered, non-persisted survey at `5ad7f5a61` completes **189 totals / 554 scopes** in
189.5 seconds, exit 0. It finds **25 totals >5% versus rounded main**, and nine >5% versus exact
`3ea7dc739`. Aliases, nineteen absent/one new historical scope and operation/fixture mismatches
are recorded name-by-name. The **16K delete/compact increase 43,376,650 → 15,498,702,117** was
unexplained at that checkpoint; the pinned follow-up below attributes it without accepting or
recovering the cost. Being near main's rounded 15.54 B is not proof of equivalent physical work.

`e596eadae` retains complete descriptor-row validation but serializes only the changed suffix
for in-place insertion. Pressure-stat setup at 256 labels falls **303,322,427 → 276,081,039**;
the actual pressure scope stays **36,157**. Native bucket write bytes fall **4,580,248 → 199,008**
with unchanged reads, write calls and growth. This is not a changed pressure policy.

Actual nonzero-property witnesses reject `0b6673a0d`: tombstones invalidate slot-as-value-ordinal
mapping, callbacks lose `Break`, descending values misalign, reversed replay loses target identity,
and reused scratch leaks another label's values. `8a6bafd1b` restores checked dense admission,
unifies ordered emission, propagates termination and retains canonical replay order. Widths 8/33,
retained independent runs, prefix/run holes, scratch reuse and selected targets are covered.
Incoming hybrid instructions are **126,123 → 111,370**, property-first/selected-topology
**280,180 → 266,211**, repeated nonzero batches **1,275,736 → 1,118,925**. The matching property-first
native probe preserves exact output and memory traffic; no per-read instruction price is inferred.

Final library **658/0**, strict library clippy and scoped formatting pass. Mixed-hub traversal
remains **790,432 / 557,118**, with unchanged exact rows and native role/read-length histograms.
Neither broader nonzero-property recovery nor mixed-hub attribution is complete. All canonical
full artifacts still describe `3ea7dc739`; only the new survey describes `5ad7f5a61`, and only named
focused controls describe later code. The shared ten-minute observation budget was exceeded
(approximately 726 seconds reconstructed); further validation was stopped. Main remains unmerged.
[Complete evidence, exact CSVs, negative diagnostics, commands and remaining gates](evidence/adr0097-current-survey-hybrid-batches.md).

#### Descriptor count rejection and pinned 16K compaction work

`bc5c7d37f` moves existing updated-row preparation before the in-place descriptor suffix write.
At the actual maximum count with backed slack, front/middle/tail insertion now returns exact
`RowDegreeOverflow` with all backing memories and vertex bytes unchanged; the owner-level
witness fails on old code. Full old-row validation and successful insertion tests remain.
Final library **659/0**, strict library clippy and scoped formatting pass; production Lizard CCN
is unchanged. This is a store rejection boundary, not broader public-write atomicity acceptance.

Fresh pinned probes record 16,384 seeded identities, every even-slot removal, all odd survivors
and exact final canonical rows in both directions. `3ea7dc739` seeds prefix 15,632 plus run 752;
`5ad7f5a61` seeds prefix 16,384 without a run. The former returns Finished after **8,192 unreported
ordinal changes**; resetting the probe oracle to packed output concealed the observer-contract
failure. The latter reports 8,192 moves. Canonical target/order agreement is not movement parity.
The benchmark supplies zero on every step, ignoring `moved.new_slot_index`; production whole-span
and deferred drivers already advance it. Both historical canbench totals remain exact, but the
67M-read derivation belongs to that benchmark cursor/workload, not all production execution.

The new cumulative ledger records **259.983 / 600 seconds**, including failed diagnostics and
final validation. Stale shared-target output and a path-alias setup failure are excluded from
candidate evidence. The earlier approximately 726-second breach remains disclosed. All nine
historical increases, broader atomicity and lifecycle gates remain open; main remains unmerged.
[Exact phases, movement traces, source provenance and remaining correction decision](evidence/adr0097-descriptor-rejection-crossover.md).

#### Compaction movement publication correction

`15ceaabf1` reproduces current-head retained-run + declined-fold completion with actual backing,
not the present 16K seed. Two witnesses fail on `d4c59ff8b`: Finished silently remaps fourteen
survivors, and deferred external identities cannot be resolved at the new keys. Both pass after
the fix, across value widths 8/33, prefix/run holes and independent value extents. A second slab
label still delivers three bounded single-edge moves; a neighboring vertex is unchanged.
The retry witness distinguishes queued failure from successful completion and prevents replay
on the next maintenance tick.

The owner and peer-cascade call sites now share physical-only packing. The shared materializer/
publisher no longer offers a logical-packing/folding mode; the unused move collector and whole-
prefix encoding path are deleted. No new queue, cache, layout, threshold, growth or fold policy
is introduced. Existing required folds, exact rejection, cursor fallback and interleaved-delete
contracts are retained. Filtered `compact` **81/0**, `relocat` **11/0**, `rewrite` **10/0**, and final
retained witnesses **2/0** pass (overlapping filters, not additive totals); strict library clippy
and scoped formatting pass. No full library/benchmark sweep or persistence was run in this slice.

On the same sixteen byte-identical pre-compaction memory images, a separate native paired
cursor diagnostic passes event replay at Finished in both orders: zero cursor **67,182,614**
reads, forwarded cursor **33,640,470**, with 8,192 moves each. Forwarding still repeatedly scans
intervening tombstones; this is neither a new production optimization nor IC instruction pricing.
All historical performance increases and wider atomicity/lifecycle/landing gates remain open.
[Commands, cumulative budget, original/corrected diagnostics and provenance](evidence/adr0097-compaction-movement-publication.md).

Bounded review approved `15ceaabf1` + `4912fa786` for movement publication only; the reviewer
did not run Rust validation. The subsequent [bounded-read baseline attempt](evidence/adr0097-compaction-bounded-read.md)
on `af063ea42` timed out before a complete canbench/native baseline or small-fixture runtime
result. Only local stale comments were corrected; no finder optimization or executable test
change was retained. It provides no new IC total, paired work equivalence or performance acceptance.

The [owner-layer follow-up](evidence/adr0097-left-pack-bounded-reads.md) adds three small witnesses
(`761b2a808`) and retains bounded, slab-only prefetch (`aab46e51f` + `ea12e7c12`). The unchanged
16K IC fixture improves **15,498,138,427 → 7,185,627,417 instructions (−53.6355%)**, with identical
movement traces through Finished. Final compaction-filter tests pass **84/0**; no full suite or
persisted survey was run. The new read path excludes tree descriptors because their stored width
is logical extent, not initialized root-array width. Bounded slab-read batching/evidence through
`56d381b3f` was approved without granting overall acceptance.

The separate [tree-compaction correctness slice](evidence/adr0097-tree-compaction-admission.md)
proves that a slab-triggered whole-span job reaches a tree sibling. Before repair, Insertion
truncated native extent 1024→1020; Unordered reported a bogus tree move 1023→3. `90757825c` fixes the shared
one-step owner to admit slab logical compaction before either selector, cursor fallback or
metadata finalization. Three tiny-only gates were removed; tree/tiny rows advance unchanged.
Real-backed witnesses preserve all native tombstones, property identities and LTB/root bytes,
replay exactly seven slab events, and finish after physically relocating the root region.
Selection-only repair still fails both witnesses. Final scoped compaction tests pass **86/0**;
independent review of this correctness slice remains open. Separate tree writer/header-validation
findings are tracked as GAP-2026-09-22-001/002 above.

Decode work and both cursor strategies remain quadratic; broad performance, atomicity, space,
PocketIC and landing acceptance remain open. The earlier incomplete evidence and corrected failing plan logs
are preserved, not relabeled.

### GAP-2026-09-20-008 — The compaction step spins when a run-backed bucket's fold needs span room

**Historical diagnosis below:** it describes earlier single-policy experiments. The current step
has an explicit span policy; a non-forcing declined fold may retain ordinals and complete, but
may not silently repack them at Finished. Required-growth/retry and broader failure-atomicity
acceptance remain separate. See the movement-publication correction above.

- **Historical status:** Open, attributed 2026-09-20 on the `spill-swap` branch. It is the delete
  family's regression (`bench_l_s2_h_del_1024` 127.2M -> 266.5M, `bench_l_s2_h_pt_d_1024`
  17.8M -> 52.0M, `bench_l_s2_h_scn_d_1024` 17.7M -> 51.9M, `offset_workload_baseline_*`
  +24.7% / +37.0%).
- **Owner:** `compact_vertex_edge_span_one_step` (compact.rs, the `resume_bucket_index == 0`
  run-handling block) and the span-resolution policy it reaches.
- **Mechanism (code path, measured).** The delete benches' measured region is 512
  `remove_edge_matching` plus the compaction loop `compact_vertex_edge_span_until_overflow_or_done`,
  which calls `compact_vertex_edge_span_one_step` and resets its cursor on
  `OverflowRewrite`. That step, whenever the cursor is at 0 and *any* bucket owns a run,
  folds the first such bucket with `compact_label_bucket_overflow_to_slab`; when the prefix
  has no room for the run's rows it grows the span with
  `rebalance_vertex_edge_span(vid, Some(bucket_index), spill_rows, true)` and **recurses**.
  That rebalance resolves its base under `SpanResolutionPolicy::SlackMayBeDropped`, so when
  the pinned leaf cannot place the wider span it silently keeps the *old* span
  (`finish_span_resolution` returns the vertex's current width). The fold therefore fails
  again, the span never grows, and the recursion repeats: one delete's compaction loop
  rewrote the span and relocated/slid the leaf ~56 times. Scope calls for
  `bench_l_s2_h_del_1024`: `labeled_leaf_try_expand_in_place` 1 -> 56,
  `labeled_leaf_weighted_slide_commit` 1 -> 56, `labeled_rewrite_read_and_plan` 3 -> 58,
  `labeled_vertex_write_edge_runs` 3 -> 58, `labeled_leaf_materialize_plans` absent -> 52,
  while `labeled_remove_edge_skip_leaf` stays 512 on both sides. `main` never enters the
  block at all: with no runs the `find(...)` matches nothing and the step goes straight to
  per-bucket packing.
- **What the third measurement showed (2026-09-20, after the owner asked for the per-path fix).**
  The step cannot serve both callers, and the reason is the cursor contract itself: the
  tombstones the deferred cursor packs are *beyond the prefix* (the run's rows), so packing
  them requires the fold, and the fold requires span room. Measured per path:
  - **Fall through to prefix-only packing** (no growth): `compaction_cursor_*` still fail -
    0 moves where 4 are required, because the prefix alone has no tombstones to pack - and
    `edge_overflow_compaction_does_not_fold_inline_property_bytes_log` fails too. 620/3.
  - **Grow with `SpanResolutionPolicy::Required`** (a real relocation, one attempt per call):
    the cursor tests pass, but `directed_insert_deferred_materialize_via_wrapper_retry_after_drain`
    fails - "width published: left 0, right 8" - the required rewrite pre-empts the pending
    width materialization and leaves the row empty. 622/1.
  - A guard that stops `finalize_bucket_slab_metadata` from shrinking or clearing a run-backed
    bucket's prefix (its ordinals are `prefix_len + i`, so shrinking truncates them) is
    required by *both* variants and is correct on its own; it is kept in the note rather than
    landed because no variant that needs it passes yet.
  The sharp policy question is therefore: the compaction's cursor contract requires folding
  run-backed buckets, which requires forcing span room (a leaf relocation). Should that
  forcing be (i) an atomic required rewrite that cannot leave a half-published row, or
  (ii) a per-caller policy the step carries, so the deferred cursor may relocate while a
  delete-driven compaction may not? The step currently has no policy parameter; both callers
  share one behaviour, which is why every single-behaviour fix breaks one of them.
- **Two candidate fixes, both measured, both rejected as guesses:**
  1. **Skip the bucket when the fold cannot be placed** (advance the cursor instead of
     growing). Breaks the compaction's contract: `compaction_cursor_packs_alternating_tombstones_across_steps`
     (0 moves where 4 are required) and `compaction_cursor_survives_interleaved_delete_in_packed_prefix`
     (`[2,4,6,8]` where `[4,6,8]` is required - the finalize truncates a live edge). The
     cursor path *requires* the fold to happen, so room must be made for it.
  2. **Grow with `SpanResolutionPolicy::Required`** (a real relocation, once). Reaches the
     deferred path's partial-mutation hazard:
     `directed_insert_deferred_materialize_via_wrapper_retry_after_drain` publishes a width
     of 0 where 8 is required, i.e. the required rewrite releases the old span before the
     new one is committed, so a failed rewrite leaves the row empty. The maintenance policy
     exists precisely to avoid that.
- **Decision needed (owner):** the compaction must fold run-backed buckets (its cursor
  contract says so), so the room is *needed* rather than speculative - but obtaining it
  either spins under the maintenance policy or risks a partial publish under the required
  one. Which of the two the compaction should carry - a bounded growth that reports
  "cannot compact yet" without mutating, or a required rewrite made atomic - is a policy
  call this slice should not make on its own.

### GAP-2026-09-20-006 — Committed-space audit: eagerly grown overflow-log capacity and per-vertex descriptor slack

- **Status:** Open, measured 2026-09-20 while auditing for the same "wasted data area" class as
  GAP-2026-09-20-005 (the promotion's dead prefix). Numbers are page counts from
  `Memory::size()` (whole 64 KiB WebAssembly pages) plus the graph's own accounting.
- **Owner:** `ic-stable-lara` per-leaf overflow log (`lara/edge/log.rs`) and per-vertex bucket-row
  slack (`labeled/bucket_store.rs`).
- **Observed behavior (5000 vertices, one edge each, single label, segment 16):**
  | | value |
  |---|---|
  | committed pages, empty graph | **30 pages ≈ 1.9 MB** (16 memories' 1-page minimums + the stores' initial growth) |
  | committed pages, after 5000 tiny edges | **66 pages ≈ 4.2 MB** (buckets 15, log 11, edges 5, vertices 2 pages) |
  | actual edge data | 5000 × 4 B = **20 KB** |
  | bucket descriptor rows reserved | **25,000** for 5,000 buckets (5 rows per vertex: 1 live + 4 slack = 116 B/vertex, physically committed) |
  | overflow-log capacity declared | **87,040 entries = 696 KB** for a workload that used **0 entries** |
  | edge-slab cover / resident | 0 / 0 (tiny truly occupies no edge slab) |
- **Two scaling wastes:**
  1. **Per-leaf overflow-log capacity is eagerly committed.** `DEFAULT_MAX_LOG_ENTRIES = 170` per leaf
     (`log.rs`), stride `4 + E::BYTES` = 8 B, and `grow_segment_count_to` ensures the *whole*
     `segment_count × 170 × 8 B` region is backed at init/reopen. At segment 16 that is
     **1,360 B per 16 vertices = 85 B/vertex**: 1M vertices → ~85 MB, 10M → ~850 MB, committed
     whether or not any leaf ever spills. The log is only needed for leaves that actually overflow
     the slab window, and the earlier log-pressure measurement (Finding C, ~1.1K instructions per
     append, folds ~43K) gives no evidence that the capacity must be reserved graph-wide.
  2. **Per-vertex bucket-row slack commits 4 extra 29-byte rows (116 B) per vertex with any label.**
     The slack is consumed by later label inserts without an in-segment rewrite (the reason it
     exists: `insert_label_bucket_at`'s fast path), so the fix is a policy/tuning question, not a
     delete: measure the descriptor-rewrite cost it saves (the vertex-segment rewrite path) against
     the 116 B/vertex.
- **Also noted (fixed-cost, likely inherent):** every one of the 16 stable memories costs a 1-page
  minimum (≈1 MB at init), and the edge/lb stores each grow to ~320 KB before any payload; merging
  small meta stores or lowering the initial growth is a layout-table decision, not a local change.
- **Expected or needed behavior:** grow the log's entry region lazily (per touched leaf, or with a
  high-water counter) so tiny/slab-only graphs commit no log entries; re-tune the bucket-row slack
  with a measured insert-vs-rewrite trade; then re-run the audit probe
  (`/tmp/space_audit_probe.rs`) to confirm the delta.
- **Post-swap measurement (F8, 2026-09-20), same shape and same probe on both sides**
  (segment 16, `InitialCapacities::uniform(256)`, 5000 vertices x 1 edge, 5000 buckets, all tiny:
  cover 0, resident 0, zero overflow usage; pages are `Memory::size()` over `FailpointMemory`,
  which reports whole 64 KiB pages - `VectorMemory::size()` reports written bytes and was the
  earlier probe's mislabelling):

  | region | `main` 9a64049f4 | `spill-swap` 1ed1f9843 |
  |---|---|---|
  | **committed pages, total** | **66** (4,325,376 B) | **56** (3,670,016 B) |
  | edge log / spill arena (mem 6) | 11 pages (720,896 B) | **1 page** (65,536 B) |
  | log capacity declared | 87,040 entries = 696,320 B, **0 used** | no log state exists |
  | spill arena runs / rows used | - | **0 runs, 0 rows** (the page is the store header) |
  | bucket descriptors | 25,000 rows x 29 B = 725,000 B | 25,000 rows x 36 B = **900,000 B** |
  | descriptor delta | - | **+7 B/row = +175,000 B** (absorbed: buckets stay 15 pages both sides) |
  | buckets promoted to tree | - | **0** |
  | other regions (identical both sides) | vertices 2, buckets 15, ipb_log 16, edges 5, edge_span_meta 1, free-span stores 1/3/1/3/1/3, ipb_slab 1, value_blobs 1, ltb 1 | same |

  The expectation is **confirmed**: `main` grows its per-leaf log region to the declared capacity
  (696 KB, 87,040 entries, zero used) for a workload that never spills, while the swap grows no log
  region at all and zero arena rows - it pays +7 B per descriptor row instead, and for this shape
  that delta fits inside the page the descriptors already had. Net: **-10 pages (-655,360 B)**.
  The values plane's own `ipb_log` (16 pages, 1,048,576 B) is unchanged on both sides: it is the
  values twin's log, still awaiting the final-row slice, and it shows the same eager-capacity shape
  this entry describes.
- **Slice 5 measurement (2026-09-20), same shape and same probe, three heads side by side**
  (segment 16, `InitialCapacities::uniform(256)`, 5000 vertices x 1 edge, 5000 buckets, all
  tiny: cover 0, zero overflow usage; pages are `Memory::size()` over `FailpointMemory`):

  | region | `main` 9a64049f4 | `spill-swap` before slice 5 (36 B row) | `spill-swap` 5c/5d (34 B row) |
  |---|---|---|---|
  | **committed pages, total** | **66** | **56** | **41** |
  | edge log / edge spill (mem 6) | 11 | 1 | **1** |
  | values ipb_log / values spill (mem 13) | **16** | **16** | **1** |
  | bucket descriptors | 25,000 x 29 B = 725,000 B | 25,000 x 36 B = 900,000 B | 25,000 x **34 B = 850,000 B** |
  | buckets memory | 15 | 15 | 15 |

  Both eager reservations the entry describes are gone: the edge log's 10 pages went with
  the edge swap, the values log's **15 pages** (16 -> 1, the values spill arena's header) with
  slice 5, and the descriptor reached its final 34-byte row. Net over the audit shape:
  **66 -> 41 pages (-25)**, of which slice 5 is **-15 pages** and the 34-byte row takes back
  50,000 B of the interim 36-byte row's 175,000 B. Against `main` the descriptor is
  **+5 B/row (+125,000 B)** - the price of the two run ids - bought back many times by the
  25 pages of never-used capacity the graph no longer commits.

- **Next decision:** decide whether to fold this into the same space-first slice as
  GAP-2026-09-20-005 (both are "committed bytes we never use") or take it as its own slice with the
  bench for the slack trade.

### Incident 2026-09-20 — `git stash` + `drop` in the shared worktree (recovered)

Whlie diagnosing GAP-2026-09-20-005 a `git stash` was used to bisect and then dropped, which swept up
**twelve uncommitted files belonging to other workstreams** (`crates/graph/src/facade/**`,
`crates/graph/src/index/canonical_export.rs`, `Cargo.lock`, the `deferred.rs` / `iter.rs` comment
hunks, `pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, `design/adr/0050` / `0059`). All of
them were restored from the still-reachable dangling stash commit `c2fae9e5b` (`git fsck
--no-reflogs --unreachable`), verified back in place, and the suite is green again (611/0). No
foreign content had been committed: the one shared file committed here (`design/adr/README.md`)
contains only this workstream's ADR 0096 row.
**Discipline for this worktree:** never `git stash` (or any tree-wide state operation) while other
agents have uncommitted work; bisect with file copies instead (`/tmp/...`), as the rest of this
session did.

### GAP-2026-09-20-005 — Log-fold span growth can extend a vertex cover over a leaf mate (K=4 exposes it)

- **Status:** **Partially fixed 2026-09-20 (`7ad12b230`)**, and the *fold* half is **closed by
  deletion 2026-09-20 (ADR 0097)**: the fold-prelude path and the whole log-fold machinery are gone,
  and a per-bucket run is compacted into its own prefix (with a span-growth retry), so no fold can
  extend a vertex cover over a leaf mate. **The second path is still open and pre-existing**: the
  GAP-006 audit probe's skew phase (`/tmp/space_audit_probe.rs`, the 160/48/24/12/8/3/1-degree shape
  in a `segment16` graph) trips the same `assert_no_labeled_leaf_mate_overlap` guard at
  `leaf_pin.rs:534` ("leaf-mate edge spans overlap in leaf 328: vid 5255 reserves up to 1384 but vid
  5248 starts at 1184") on `main` **and** on the `spill-swap` head, so the swap neither causes nor
  fixes it - it is a span-reservation defect in the promotion/skew path, not a log defect, and it
  needs its own slice. Reproduced 2026-09-20 on both heads.
  Discovered while auditing
  production capacity/latency balance;
  regression-adjacent to ADR 0096 §3b Phase 2 (`91575bc29`), which made it reachable in a
  production-shaped skew where the K=3 wire was not.
- **Severity:** P0 correctness (cover overlap ⇒ a later release/density decision can act on a
  mate's slots; the debug-only guard `assert_no_labeled_leaf_mate_overlap` is the only detector,
  so **release builds run on silently.**
- **Owner:** overflow-log fold / span-growth path (`compact.rs` fold + cover growth; the same
  class the promotion path already guards with its `mates_free` check).
- **Observed behavior:** after a hub's span fills, the per-leaf overflow log fills and folds into
  the bucket span. The fold grows `stored_slots` **in place** without checking the leaf's other
  covers, so the grown span walks over mates that were placed after it.
- **Minimal reproduction** (deterministic, single label, Insertion policy, production
  `segment_size = 16`, `InitialCapacities::uniform(256)`, 16 vertices in one leaf, `TestEdge`
  4-byte): degrees `[8, 8, 8, 8, 64, 64, 64, 64, 64, 64, 64, 64, 512, 512, 512, 2048]`, mates
  inserted first, then the 2048-degree hub. The hub promotes at its 5th insert into the 8-slot
  gap `[356, 364)` (legal), then the fold grows it to 175 slots in place:
  `vid VertexId(15) reserves up to 531 but vid VertexId(5) starts at 364`
  (`leaf_pin.rs` guard, leaf 0, block `(256, 2288)`).
- **K=3/K=4 split (measured):** the same probe on `1c277d95c` (R4, K=3) **passes**; on
  `91575bc29` (K=4) it fails. The missing mate check is not new, but the K=4 promotion shape
  (span 5 instead of 4, promotion at the 5th insert instead of the 4th) changes which free run
  the hub lands in, turning a latent hazard into a reachable one.
- **Expected or needed behavior:** the fold's span growth must be mate-disjoint (check
  `labeled_leaf_occupied_spans` for `[base, base + grown)`), and a collision must relocate/grow
  the leaf block and retry — the same bounded-retry pattern as
  `promote_tiny_to_slab_with_growth`. Promote the debug guard to a checked error on the growth
  path (fail closed) so release builds cannot proceed on an overlapping cover.
- **Impact:** data-loss class (an overlapping cover can free a mate's slots on delete/relocate),
  plus density/audit misfires. Also blocks any honest capacity-vs-latency tuning of the tiny/slab
  boundary, since the geometry it would tune is currently not always valid.
- **Fold-path fix (`7ad12b230`):** `prepare_vertex_edge_span_for_overflow_log_fold` now requires
  the wider range to be mates-disjoint (reusing `try_labeled_vertex_edge_base_in_pinned_leaf`),
  relocates the leaf block on a collision, and fails closed otherwise. Regression
  `fold_growth_stays_mate_disjoint_across_span_growth` (the minimal reproduction above) fails on
  the block-only check and passes with the fix.
- **Second path still open (diagnosed 2026-09-20 on the same commit):** the wider G5-shaped skew
  (256 vertices, degrees 1..2048, segment 16) still trips the guard in leaf 15:
  `vid 255 reserves up to 1568 but vid 240 starts at 400`.
  The writer is the **tree promotion**, not the fold:
  `promote_bucket_if_needed` -> `promote_bypass_to_tree_mode[_impl]` publishes the new tree
  descriptor with `edge_start = allocate_span(combined_root_len)` (a fresh, globally allocated
  LEG root region — slot 256 in this run, i.e. *outside* the leaf block `(400, 3664)`), and the
  vertex row's `stored_slots` keeps its **slab-era cover** (1312). `tree_mode_insert_edge` then
  writes the same stale cover with `degree = stored = 1151`. The audit reads `vertex.stored_slots`
  as the reservation, so the stale slab cover looks like a span reaching from 256 across the
  leaf's re-tiled mates.
  Two facts fall out, and ADR 0088 §3 already prescribes which way to fix them — "the
  rope/PMA/placement layers treat the root span exactly like a small slab bucket ... and stay
  mode-blind":
  1. `promote_bypass_to_tree_mode_impl` never re-bases the vertex cover on the resident-geometry
     SSOT (`bucket_physical_resident_slots(bucket)` = `combined_span_region_len` = `root_len`
     slots for tree, not `degree`). The published cover therefore describes a region the tree
     bucket does not own.
  2. The leaf-cover model itself (one `(base, cover)` interval per vertex, used by the audit, the
     slide, and release math) cannot represent a vertex whose tree root was allocated outside the
     leaf block while sibling slab buckets stay inside it. Per ADR 0088 §3 the root span is *not*
     a special case: it is an ordinary vertex-local span, so it must be reserved through the same
     vertex/leaf tiling the slab path uses, and the cover re-based on the resident length —
     rather than teaching the cover model an out-of-block representation.
  Also verified while tracing: the relocation slide re-tiles this leaf correctly
  (`TMPSLIDE`: hub `old2752+688 -> 2749+979`, later `-> 2752+1312`, inside the block), so the
  slide is not the writer; the guard's span is the *stale vertex cover*, not a moved span.
- **Minimal variants tried and rejected by measurement (2026-09-20, same commit):**
  (a) *re-base the vertex cover on `bucket_physical_resident_slots` only* — breaks two existing
  invariants, measured: `gap_tree_full_path_growth_past_5728_releases_only_owned_regions` and
  `tree_mode_leaf_actual_counts_slab_edges_only` fail with `leaf 0: PMA total mismatch
  (store vs labeled geometry) left: 1040 right: 16`, and
  `single_label_log_fold_reserves_edge_only_tail_headroom` fails on
  `vertex.stored_slots >= segment_size`. A cover is a vertex's *share of the leaf block* (resident
  plus distributed slack), not the bare resident sum, so shrinking it to the root region breaks
  `Σ covers == leaf total` and the headroom floor.
  (b) *relocate the leaf once after the promotion* (pinned or not) — with the shared
  position helper made tree-aware it fixes the reproduction, but the slide itself can fail
  (`CollectAllocationOverflow`) on other shapes (`directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`)
  and it writes the new block start before the per-vertex commit, so swallowing the error could
  leave a half-moved leaf; propagating it after the descriptor commit turns a successful promotion
  into an error. Not acceptable as the minimal variant.
  Conclusion: no small fix exists. The root must be reserved **inside the vertex's span** from the
  start (so the cover, the block tiling, and the release math all stay consistent by
  construction), which is the ADR-prescribed route below.
- **In-span reservation attempt (2026-09-20, implemented then reverted):** Phase 1 was rewritten to
  reserve the combined root region *inside* the vertex span (property sizing first; then
  `rewrite_vertex_edge_span(vid, Some(bucket_index), combined_root_len, …)`; root base =
  `bucket.edge_start() + stored_slots` after re-reading the bucket; every span-release rollback
  dropped, LTB releases kept). Outcome: the reproduction passes (the target defect is fixed), but
  three interactions remain, so the attempt was reverted to keep the tree green:
  1. the re-laid vertex span can straddle the leaf block end
     (`fold_growth_stays_mate_disjoint_across_span_growth`: hub cover `[3758, 5070)` against block
     `(256, …3920)`) — the vertex-span rewrite's out-of-block escape hatch
     (`tail_append_labeled_edge_base` while a relocation is in progress) or its sizing can exceed
     the block, which the cover/tiling invariants forbid;
  2. `batch_plan_with_mixed_slab_and_tree_runs_rejects_only_tree_run` fails with
     `CollectAllocationOverflow` (the pre-transcription rewrite can fail on batch-shaped fixtures);
  3. `demote_atomic_on_failure` trips `LtbRawBlockStore::release(0): block is already Free` —
     **resolved** (`d80504c2e`, test-only): the fixture's premise (the property restore reads an
     unminted id from whatever follows the edge root) was placement-dependent; the test now writes an
     explicit unminted sentinel and asserts the descriptor is byte-identical after the failed demote.
     Details in the original narrowing below, kept for the record.
     Narrowed by instrumentation (2026-09-20): `physical_depth` is 1, so the depth-2 interior walk
     is skipped; the two releases of block 0 come from the test's **two demote calls on a synthetic
     state** (`w = 4` patched without minting the matching LPB, bucket relocated to
     `edge_start = 4352`, `stored = 4096`). With the in-span change the *first* call gets past the
     property visit and completes Phase 5 (releases the leaf ids), so the second call re-releases
     them — i.e. the test's premise ("the first call fails before any release") is
     placement-dependent, not an invariant. Next attempt: decide whether the fixture should build a
     legitimate LPB (or patch the depth/width consistently) so demote atomicity is pinned on a state
     that does not depend on where the promotion placed the root.
  Re-run after `d80504c2e`: interaction 3 is gone; the remaining failures with the in-span change are
  (1) and (2), with (1) narrowed further — the straddling cover observed in
  `fold_growth_stays_mate_disjoint_across_span_growth` (`base=3758 end=5070 leaf=(256, 3664)`) still
  carries the **pre-promotion slab width** (1312), so it is not the `tail_append_labeled_edge_base`
  escape hatch. Instrumenting `commit_vertex_edge_span_layout` (the slide/rewrite commit) shows the
  hub is **not** among the vertices that path republishes in that workload (only vids 0..11 appear,
  their spans summing 612 slots inside the block), so the 1312 cover is written by another site.
  Instrumentation then cleared the fold-prelude suspect and found the real shape (2026-09-20):
  - `prepare_vertex_edge_span_for_overflow_log_fold`'s relocate branches are fine — the relocation's
    slide *does* republish the cover, and the printed state is consistent
    (`after-relocate vid=15 stored=1312 base=Some(2608) leaf=Some((256, 3664))`, i.e. the span ends
    exactly at the block end).
  - The inconsistency is **tree-bucket sizing inside the vertex-span layout**. After the promotion,
    `edge_start` is the LEG *root* offset (3758) while the vertex cover still carries the vertex's
    block share (1312), because the layout helpers size a tree bucket by its *logical* degree:
    `read_and_plan`'s `total_live` and `calculate_label_edge_span_positions_by_resident_slots`'s
    `resident`, plus the materialization slice length at `compact.rs:984` (a tree bucket has no slab
    rows there — reading `stored`-many rows panics with `range end index 4600 out of range for slice
    of length 8`). ADR 0088 §3 names the intended shape: promotion publishes the descriptor and then
    "releases the old edge span (via the vertex-span rewrite)", so the vertex is re-laid with the
    tree bucket contributing `bucket_physical_resident_slots` (its root region) — no Phase-1
    restructuring needed.
  - Working files from this pass: `/tmp/promote_postpublish_attempt.rs` (post-publish rewrite) and
    `/tmp/compact_treeprep_attempt.rs` (tree-aware planning + positions, which then hits the
    materialization panic).
- **Class-level reframing (2026-09-20) and the structural step:** every ①/② failure above is one bug
  class — *a mode-specific field (`degree`, `stored_slots`, `edge_start`) read inside the mode-blind
  geometry layer* (ADR 0088 §3 keeps that layer mode-blind; ADR 0096 §7 keeps mode decisions at the
  dispatch points and transitions). The structural fix is one SSOT, `bucket_resident_region()`
  (tiny → `None`, slab → `(edge_start, stored_slots_raw)`, tree → `(edge_start,
  combined_span_region_len)`), with `bucket_physical_resident_slots()` delegating to it, and the
  geometry sites expressed through it so the wrong read cannot be written: `read_and_plan`'s
  `total_live`, `calculate_label_edge_span_positions_by_resident_slots`'s `resident`,
  `label_buckets_allow_contiguous_slab_copy` ("every bucket's region *is* its slab prefix"), and the
  `try_labeled_vertex_edge_base_in_pinned_leaf` block-bound check (which was missing its *lower*
  bound). Implemented and verified green in isolation.
  Remaining piece, pinned precisely: `rewrite_vertex_edge_span`'s inline copy path (the one built
  from `per_bucket`/`buf`, not the `per_bucket_raw` path) cannot move a tree bucket's LEG root array,
  so the post-promotion re-lay still fails with `CollectAllocationOverflow` after four relocations.
  Next attempt: give that path the materializer's tree branch (move the root bytes by anchor), then
  add the promotion's Phase 3e (re-base the cover on the SSOT + `rewrite_vertex_edge_span(..., force_slack_grow = true)`)
  and the two regression tests. Working copies: `/tmp/ssot_{compact,promote,bucket,leaf_pin,graph}.rs`.
- **Space-first attempt (2026-09-20, C = reserve the root inside the vertex span):** Phase 1 now
  reserves `combined_root_len` through
  `rewrite_vertex_edge_span(vid, Some(bucket_index), combined_root_len, …)` and writes the root at
  `bucket.edge_start() + stored_slots`; the old prefix is released by the existing Phase 3c, so no
  space is forfeited (unlike the reuse variant). With the SSOT migrations, the tree-aware copy arms
  in both inline loops, and the lower bound in `try_labeled_vertex_edge_base_in_pinned_leaf`, the
  **skewed-leaf regression passes with C alone**.
  What C alone does not do: the vertex cover still measures the vertex's share from the *released
  prefix base*, while the tree bucket's `edge_start` is now the root — and the debug guard (and the
  layout) reconstruct the vertex's span from the first non-tiny bucket, so the reconstructed span
  straddles the block end (`base=3758 end=5070 leaf=(256, 3664)`). So C needs the cover normalized
  too; the model has nowhere to record the vertex's span start (the 29-byte descriptor has no spare
  field, and the row's reserved bits are too narrow).
  **Copy-free refinement (2026-09-20):** the prefix never needs to be *moved* — it is dead after the
  transcription, so the root can be written over the prefix's **head** (`edge_start` unchanged), which
  also keeps the vertex's span start (and therefore the cover arithmetic) exactly where it was; the
  single post-promotion layout pass then only *shrinks* the vertex's share to shed the dead tail
  (root bytes move by anchor, the vertex's other buckets shift down). Implemented that way
  (`/tmp/d_promote.rs`, `/tmp/d_compact.rs`): Phase 1 allocates nothing, Phase 3e re-bases the cover
  on the resident SSOT and re-lays.
  **Root cause of that failure (found 2026-09-20):** `calculate_label_edge_span_positions` (the
  degree-weighted positions helper used by the *planning* path) computes its slot budget as
  `Σ bucket.degree()`, so a promoted bucket demanded its full logical count of a span sized for its
  root region — `gaps = span_slots - effective_live` underflowed into `CollectAllocationOverflow`.
  With `effective_live` taken from the resident-region SSOT (weights stay label-degree-based, which
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` pins), **both regression tests pass** with
  the copy-free shape.
  **Blast radius measured (13 tests, classified):**
  - *design contract change* (expected, re-express): `promote_edge_start_points_to_leg_offset`,
    `promote_pre_and_post_edge_start_differ`, `promote_publish_phase_atomic_descriptor_write`,
    `promote_succeeds_when_alloc_space_at_cap` (the root now reuses the prefix head, so
    `edge_start` is unchanged and no span is allocated — the "fresh LEG offset" proxies no longer
    hold).
  - *audit model* (needs the leaf-level slide, see below): `gap_tree_full_path_growth_past_5728_releases_only_owned_regions`
    and `tree_mode_leaf_actual_counts_slab_edges_only` fail with `leaf 0: PMA total mismatch (store
    vs labeled geometry) left: 1040` — shedding the prefix leaves its slots unassigned in the leaf,
    while the audit requires `Σ covers == leaf total`. The honest fix is to re-tile the **leaf**
    (slide / relocate) in Phase 3e instead of only the vertex, so the freed slots are redistributed
    to the leaf's other vertices.
  - *expectation shifts* (policy preserved, numbers move): `labeled_segment_relocate_reuses_free_span`,
    `vertex_edge_span_rewrite_weights_slack_by_label_degree`,
    `single_label_log_fold_reserves_edge_only_tail_headroom`.
  - *suspected real bugs* (data assertions, investigate before updating anything):
    `default_bypass_conversion_clears_vertex_edge_span_allocation` (`left: [TestEdge { target: 0 },
    TestEdge { target: 0 }]` — bypass rows read back as zeros) and
    `edge_inline_propertys_survive_rewrite_with_tombstones` (`left: [(0, 20), (0, 30)]` — property
    values zeroed); plus `cascade_at_2_20_plus_1_deepens` (`promote: CollectAllocationOverflow`) and
    the known ② `batch_plan_with_mixed_slab_and_tree_runs_rejects_only_tree_run`.
  Working copies of the passing shape: `/tmp/final_*.rs` (superseding `/tmp/d_*`).
  **Leaf-slide variant works (2026-09-20):** an earlier attempt at this looked broken because a
  script splice had landed Phase 3e *inside* Phase 1 (so the slide ran before the descriptor flip and
  the promotion looked like a no-op); rebuilding the function from the known-good shape
  (`/tmp/c_promote.rs`) with Phase 3e at the end — Phase 1 root-in-place, transcription, flip, release,
  then `rebalance_labeled_leaf_weighted_slide(vid)` — makes **both regression tests pass** and drops
  the suite to **603 passed / 9 failed** (from 13). The leaf re-tile absorbs the shed prefix slots into
  the neighbours' shares, which is exactly what fixes the audit-model pair
  (`gap_tree_full_path_growth_past_5728_…`, `tree_mode_leaf_actual_counts_slab_edges_only`), plus
  `cascade_at_2_20_plus_1_deepens` and `single_label_log_fold_reserves_edge_only_tail_headroom`.
  Remaining 9, classified: (i) *design contract, re-express* —
  `promote_edge_start_points_to_leg_offset`, `promote_pre_and_post_edge_start_differ`,
  `promote_publish_phase_atomic_descriptor_write`, `promote_succeeds_when_alloc_space_at_cap` (no fresh
  root span: `edge_start` is the prefix head, nothing is allocated); (ii) *policy/expectation review* —
  `labeled_segment_relocate_reuses_free_span`, `vertex_edge_span_rewrite_weights_slack_by_label_degree`;
  (iii) *data-assertion failures* — `default_bypass_conversion_clears_vertex_edge_span_allocation`
  (bypass rows read back as `TestEdge { target: 0 }`),
  `edge_inline_propertys_survive_rewrite_with_tombstones` (property values read back as zeros),
  `directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`
  (`CollectAllocationOverflow` in a schema path).
  **Bisect result (2026-09-20):** these three are *not* independent defects.
  Root cause pinned by three bisect steps: `promote.rs` alone is fine, `bucket.rs` alone is fine,
  `compact.rs` is the culprit, and within it the single change that reproduces
  `default_bypass_conversion_clears_vertex_edge_span_allocation` is `read_and_plan`'s `total_live`
  switching from `degree` to the resident SSOT. The reason is a missing dimension in the SSOT: a
  **compacting** rewrite materializes only the *live* rows (tombstones are packed away), so its span
  budget must stay `degree` for slab buckets — while a promoted bucket still needs its LEG root
  region. The SSOT as applied returns the tombstone-inclusive width, which over-reserves slab spans
  and shifts the published layout. Next step: make the budget compaction-aware
  (`bucket_rewrite_content_slots(bucket, compact)`: compact → slab `degree` / tree root / tiny 0;
  non-compact → the resident region) and re-run these three. Applying only the
  resident-region SSOT + planning positions fix (`79ca06e24`) keeps the whole suite green (611/0) and
  all three pass; they fail only once the promotion/leaf-tiling rework is applied on top, so they
  belong to that rework's blast radius and must be explained (or the rework corrected) rather than
  patched in place. Working copies of the rework: `/tmp/best_*.rs`; the SSOT increment is now
  committed.
  **Compaction-aware budget applied (2026-09-20):** `bucket_rewrite_content_slots(bucket, compact)`
  (compact → slab `degree` / tree root / tiny 0; non-compact → the resident region) next to the
  resident-region SSOT, wired into the planning sizing and the planning positions helper (weights
  stay label-degree-based). The rework's suite moves from 9 to **8 failures**:
  `default_bypass_conversion_clears_vertex_edge_span_allocation` is fixed. The two remaining
  data-assertion tests (`edge_inline_propertys_survive_rewrite_with_tombstones`,
  `directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`) are the rework's last
  unexplained piece; the other six are the four promotion contract tests and the two
  expectation/policy tests. Working copies: `/tmp/best2_*.rs`.
  Narrowed further (2026-09-20): in `edge_inline_propertys_survive_rewrite_with_tombstones` the
  **property values read back correctly** (20, 30) while the **edge targets read back as 0** — so the
  values slab is fine and the *edge* copy is what breaks. The call takes the **disjoint** branch
  (`moved && old_alloc > 0 && new_base != old_base`, `slab_only_bulk == false` because the bucket has a
  tombstone), i.e. the `per_bucket`-collecting loop: the collected edges are written at the published
  positions, so the next instrumentation target is the `collect_out_edges_slot_order` source against
  the positions the plan published (the pair that used to be derived from the same `degree` width; the
  compaction-aware budget changed one side of that contract).
  **Duplication removed (2026-09-20):** `rewrite_vertex_edge_span` had three copies of the
  collect-and-publish logic (two inline fast paths plus `commit_vertex_edge_span_layout`), which is
  what kept drifting. Replacing both inline paths with the materializer + commit pair
  (`materialize_labeled_vertex_edge_plan` → `commit_vertex_edge_span_layout`, the same code the leaf
  slide uses) deletes 268 lines and fixes `edge_inline_propertys_survive_rewrite_with_tombstones`
  outright. Combined with the compaction-aware budget and *without* the promotion rework the suite is
  **609 passed / 2 failed**, and both failures are the sizing/expectation pair
  (`labeled_segment_relocate_reuses_free_span`, `vertex_edge_span_rewrite_weights_slack_by_label_degree`):
  the unified path moves the bucket's whole slab prefix, so the span budget must cover the prefix
  (`stored`) rather than the live count (`degree`) — the old `degree` budget under-reserved, and those
  two tests pin the old numbers. Next step: re-derive those two expectations (with that justification)
  **But the two remaining failures are not expectation shifts** (checked 2026-09-20): with the unified
  code on HEAD, `labeled_segment_relocate_reuses_free_span` fails `assert_eq!(new_start, old_start)`
  with `left: 1280, right: 256` — the in-place leaf expansion no longer consumes the adjacent released
  span, so the relocation took a fresh block — and
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` fails
  `cold_capacity >= cold_bucket.stored_slots()`, i.e. the published tiling gives the cold bucket less
  room than its own stored width. Both are invariants, so the unified path has a real behavioural
  delta to diagnose (candidates: the plan's compaction-aware budget versus the commit's
  resident-slot position helper disagreeing now that the inline fast paths are gone; or the
  materializer's `raw` for a log-free slab bucket being the *prefix* while the plan budgeted the
  packed run). Do **not** re-derive those two expectations before that is explained. Copies:
  `/tmp/unified_compact.rs` (unified layout code to pair with HEAD's other files).
  **Last duplicate found (2026-09-20):** `rebalance_vertex_edge_span` (~2487-2660) is a *fourth* span
  layout implementation — its own snapshot loop (`stored_slots_raw` per bucket), its own
  `calculate_label_edge_span_positions_by_resident_slots` call, its own publish loop
  (`write_label_bucket_row_adaptive`) and its own vertex-cover write (`with_stored_slots(new_alloc)`).
  Test 2 calls exactly this path, which is why instrumenting `commit_vertex_edge_span_layout` printed
  nothing for it. Consolidation therefore has one more step beyond the rewrite: route
  `rebalance_vertex_edge_span` through the same plan + materialize + commit pair (or delete it in
  favour of the rewrite it duplicates), then re-check the two invariants — only then is the
  expectation question meaningful. Copies: `/tmp/unified_compact.rs`.
  **Delegation attempt (2026-09-20):** replacing `rebalance_vertex_edge_span`'s body with a call to
  `rewrite_vertex_edge_span` (deleting 137 more lines) breaks **24** tests — the two functions do not
  share semantics yet: the rebalance path must not release the vertex footprint
  (`labeled_leaf_rebalance_does_not_release_span`), it folds overflow logs
  (`labeled_leaf_rebalance_folds_overflow_log`), and several batch/deferred/inline-property tests
  depend on its ordering. So the last consolidation needs an explicit semantics decision per caller
  (footprint release, log folding, ordering) with that 24-test list as the worklist; the direction is
  still right (one layout implementation), but it is a slice of its own. Copies:
  **Classification of those 24 (2026-09-20):** they are not 24 semantic differences — **22 of them
  panic with the same `Store(CollectAllocationOverflow)`** at their `rebalance_vertex_edge_span`
  call sites (leaf rebalance, relocation, batch/deferred, mixed-label hub fixtures, slack/slide
  tests). So the dominant delta is *one sizing policy*: the rewrite's plan budgets the
  compaction-aware `stored` span while the old rebalance computed `min_required` from
  `.max(degree)`-floored residents and grew via `next_vertex_edge_span_allocation`, and the larger
  request overruns the small caps those fixtures configure. Only two failures are independent:
  `overflow_rewrite_compacts_only_log_suffix_before_slab_body` (`EdgeMoved` where the test expects
  overflow-only compaction — ordering) and
  `directed_insert_deferred_materialize_via_wrapper_retry_after_drain` (`width published | left: 0 |
  right: 8`). Next: settle the single span-budget policy for both callers; that should collapse most
  of the 24. Copy `/tmp/delegated_compact.rs`.
  **Root cause of those 22 (2026-09-20):** `Store(CollectAllocationOverflow)` is not an allocation
  cap. `bucket_store.rs`'s partition search (`find_*` over bucket slots) returns it when
  `read_label_bucket_slot(slot)` finds **no descriptor** at the slot it computed from
  `vertex.base_slot_start() + index`, and `bucket.rs` uses the same code for its checked conversions.
  So the delegated plan-based path publishes bucket rows at anchors the store-level readers do not
  look at: the descriptors exist, the lookups miss. That matches the two standalone invariants
  (`new_start` 1280 instead of 256; cold bucket's room narrower than its stored width) — all three are
  *anchor/placement* differences, not budgets. Also tested and ruled out: making the materializer
  compact-aware (`compact` now flows into it: log-free slab packs live slots via
  `pack_live_edge_slots`, log-backed drops superseded prefix slots) does not move any of the 24 —
  saved as `/tmp/unified2_compact.rs` (609/2, same two invariants). Next: compare the plan's published
  anchors against the two deleted implementations' anchors for one failing fixture, and pin the
  contract the readers rely on (`base_slot_start` + bucket index) before re-attempting the delegation.
  **The reader contract (verified 2026-09-20, `bucket.rs`):** `vertex.base_slot_start()` is the origin
  of the vertex's **label-bucket descriptor rows**, and their extent is
  `vertex.label_bucket_descriptor_span()`; `vertex_bucket_descriptor_row_end()` = `base + span` is how
  the *next* vertex's origin is derived (`ensure_vertex_bucket_row_origin`). Edge **content** is
  addressed independently by `bucket.edge_start()`, so slack/gaps between bucket contents do not move
  descriptor rows: `write_label_bucket_row_adaptive(vertex.base_slot_start(), &rows)` places descriptor
  *i* at `base + i`. `commit_vertex_edge_span_layout` therefore recomputes content positions from the
  resident buckets it is handed and is already exercised green by the two leaf-relocation call sites,
  which is why it is probably not the culprit: what differs in the delegated rewrite is the
  plan→commit inputs (`buckets`, `new_alloc`, `vertex`, `suppress_vertex_footprint_release`), the dead
  `positions` the plan still computes and drops (compiler warning `unused variable: positions`), and the
  finalize/release block after it. Next: delete the plan's dead positions, make the plan's bucket slice
  and `new_alloc` the only interface, and diff one failing fixture's descriptor row region
  (`base_slot_start` .. `+span`) before and after the delegated rebalance.
  **Correction (2026-09-20, same day):** the "anchor" reading above is **wrong**; the store-level
  search is not involved. Instrumenting all 4 `read_label_bucket_slot` miss sites in
  `bucket_store.rs` and all 44 `CollectAllocationOverflow` sites in `bucket.rs` produced **no
  output** for `labeled_leaf_rebalance_does_not_release_span`. The site is in the planner:
  `rewrite_vertex_edge_span_read_and_plan` returns `CollectAllocationOverflow` when
  `new_alloc < min_required` in the `in_window_layout` branch, and `next_vertex_edge_span_allocation`
  can return it for the non-window branch. `min_required = total_live + preferred_extra` with
  `total_live = Σ bucket_rewrite_content_slots(bucket, compact)` — the compaction-aware resident SSOT
  substituted for the old rebalance's per-bucket `.max(degree)`-floored residents. So the remaining
  delta *is* the budget after all (the earlier budget hypothesis was right; the anchor detour was
  mine). Next: for one failing fixture print `old_alloc`, `total_live`, `preferred_extra`,
  `min_required` and the per-bucket resident terms from both formulas, then settle the single
  resident/budget definition for both callers.
  **Measured and fixed the same day:** the planner's `total_live` **did not count the overflow log**.
  In `labeled_leaf_rebalance_does_not_release_span` the fixture is a log-backed slab bucket with
  `stored_raw = 4`, `degree = 96` and `resident = 4`, so the plan asked for a span of `4` slots while
  the materializer needs `4 + 92 = 96`. Adding the chain length (`overflow_log_chain_len`, the same
  definition the materializer uses) made that test pass and moved the delegated suite from **587/24 to
  597/14**. The remaining 14 are the same under-count one layer out: the leaf/vertex *placement*
  budgets (`span_slots`, the `positions` helpers) still size from `bucket_physical_resident_slots`,
  i.e. the prefix alone, so log-backed buckets still under-reserve there
  (`mixed_label_hub_{20,33,50}`, `labeled_hub_33_labels_bounded_insert_time`,
  `batch_relocates_same_leaf_once_for_multiple_buckets`, `expanded_slab_*`,
  `labeled_relocation_preserves_all_leaf_vertices`). Next: one shared definition
  (`resident = prefix + log` for slab, LEG region + log for tree, 0 for tiny) used by the planner, the
  placement helpers and the materializer, then re-run; the four assertion-level failures
  (`labeled_relocate_commit_order`, `labeled_segment_relocate_releases_single_footprint`,
  `labeled_segment_slide_coalesces_adjacent_free`, `vertex_edge_span_rewrite_weights_slack_by_label_degree`)
  need individual review afterwards. Copies: `/tmp/delegated_logfix_compact.rs`,
  `/tmp/delegated_logfix_init.rs` (597/14).
  **Correction and next layer (same day):** the placement helpers do **not** miss the log —
  `plan_labeled_leaf_relocation` and `rebalance_labeled_leaf_weighted_slide_in_block` both already
  add `overflow_log_chain_len`. So the remaining ~10 `CollectAllocationOverflow` failures sit in
  another budget check. Sites still worth instrumenting, in order:
  `prepare_vertex_edge_span_for_overflow_log_fold` (compact.rs ~2737, folds the log while sizing the
  span), `leaf_pin.rs` (12 `CollectAllocationOverflow` sites, block/pin growth), the restart recursion
  in `rewrite_vertex_edge_span` (it drops `force_slack_grow`, so a re-plan can ask for a quantum the
  relocate did not size), and `next_vertex_edge_span_allocation` (span.rs). Suggested fixture:
  `mixed_label_hub_20_labels_500_edges_each` (smallest of the three hub fixtures) at
  `test_support.rs:446`. Landing note: the log fix alone is unobservable on HEAD (the fourth
  implementation's `.max(degree)` floor hid it), so the landing unit is unification + log fix +
  **Where the remaining insert-path failures actually happen (measured 2026-09-20):** instrumenting the
  planner's output for `mixed_label_hub_20_labels_500_edges_each` gives
  `compact=false force=true extra=0 old_alloc=576 total_live=674 min_required=674 new_alloc=1296`
  with `buckets = [(slab, stored=500, degree=500, resident=500), (slab, stored=4, degree=174,
  resident=4)]`. So the planner **succeeds** — it correctly counts the log (4 + 170) and asks for the
  next quantum (1296 > 576) — and the `CollectAllocationOverflow` is raised *after* it, on the
  **growth** path (leaf relocation / in-leaf re-root / commit), which is exactly where the fourth
  implementation differed: the delegated shim routes `rebalance` through the rewrite, whose
  `moved = new_alloc > old_alloc` sends the vertex to `plan_labeled_leaf_relocation` and re-tiles the
  whole leaf, while the fourth implementation kept its own `new_base`/release logic and
  `vertex_edge_span_relocates_within_pinned_leaf` checks. Next question, in code: on a *growing* span,
  what does the fourth implementation (`git show HEAD:...compact.rs`, `rebalance_vertex_edge_span`) do
  that the plan-based path does not — in-leaf re-root versus whole-leaf re-tile, and the
  `LABELED_REBALANCE_LEAF_RELOCATED` protocol it maintains? Read those two paths side by side for one
  growing fixture; do not sweep instrumentation across the crate again (it touched a foreign file and
  **Two contracts found by side-by-side reading (2026-09-20):** the rebalance path this replaces
  uses `resolve_labeled_edge_base_for_rebalance`, which **never fails**: it pins/relocates once and
  then falls back to `labeled_edge_base_from_first_bucket(src)` (and sets
  `LABELED_REBALANCE_LEAF_RELOCATED`). The plan path calls `resolve_labeled_edge_base_for_growth`,
  which returns `CollectAllocationOverflow` after four relocation attempts — that is the error the hub
  fixtures report, raised *after* the planner has already succeeded. Ported both rebalance contracts
  into the shared rewrite: (1) the leaf-capacity guard (`min_required > max stored in leaf` ⇒ relocate
  the leaf first) — no test changed; (2) **proactive slack is best effort** (`force_slack_grow` with
  `old_alloc >= min_required` downgrades to an in-place rewrite when the leaf cannot host the bigger
  span) — this fixed `labeled_relocation_preserves_all_leaf_vertices`, 14 → **13**. The three hub
  fixtures still fail, so their inserts need *real* growth (`old_alloc < min_required`), where
  `for_growth` errors while `for_rebalance` falls back to the first bucket's base. Next decision, with
  one bounded measurement: for `mixed_label_hub_20_labels_500_edges_each` print `min_required`,
  `old_alloc`, whether the vertex is in a pinned leaf and how many relocations each attempt needs —
  then decide whether the insert path must tolerate the same fallback (and what it does with a cover
  bigger than the owned range) or must genuinely grow. Copies: `/tmp/delegated_logfix2_compact.rs`,
  `/tmp/delegated_logfix2_init.rs` (598/13).
  **Landed (2026-09-20, `d7265949d`): partial unification on HEAD.** `rebalance_vertex_edge_span` no
  longer carries its own snapshot, position and publish loops: it materializes with
  `materialize_labeled_vertex_edge_plan` and publishes through `commit_vertex_edge_span_layout`, keeping
  only its own policy (sizing, base resolution, release ownership). Log ownership is now an explicit
  argument of that pair: `fold_logs = false` (rebalance) materializes the slab prefix alone and preserves
  `overflow_log_head`; `fold_logs = true` (rewrite, leaf relocation) materializes prefix + log and clears
  the head. That split is required by `compact_vertex_edge_span_one_step`, which folds one bucket's log
  itself and asserts stable slot indices. Suite 611/0, scoped fmt/clippy clean. Still open from the
  earlier analysis: the *full* delegation of `rebalance_vertex_edge_span` to the rewrite, because the
  rewrite resolves growth through `resolve_labeled_edge_base_for_growth` (fails after four relocations)
  while `resolve_labeled_edge_base_for_rebalance` never fails and falls back to
  `labeled_edge_base_from_first_bucket`; plus a canbench run over the publish path and a design-doc pass
  on the log-ownership contract (ADR 0096 / `design/storage/lara.md`) rather than only this ledger.
  **Validation for `d7265949d` (2026-09-20):** focused canbench from `crates/ic-stable-lara` —
  `canbench compact` 4 benches, 0 regressed (max +20 instructions, noise) and `canbench ins` 20 benches,
  0 regressed (one −0.06%, −15.72 K instructions). No persist, so `canbench_results.yml` is untouched;
  unfiltered `canbench --persist` remains a final-gate item. The log-ownership contract is now in ADR
  0096 §5 (`fold_logs` row) rather than only here.
  **Duplicate resident computation consolidated (2026-09-20, `729214852`):** "physical region plus
  overflow-log chain" was spelled out by hand in `materialize_labeled_vertex_edge_plan`,
  `plan_labeled_leaf_relocation` and `rebalance_labeled_leaf_weighted_slide_in_block`; the planner's
  copy was a fourth and omitted the log term, which *is* the bug this session chased. All three now
  call `bucket_resident_rows(leaf, bucket)`. `prepare_vertex_edge_span_for_overflow_log_fold` keeps its
  own `stored_slots() + log` on purpose (logical width after folding, which differs from the physical
  region for tree buckets). Suite 611/0.
  **Open defect this exposed (not introduced, but now visible):** clippy reports
  `rebalance_vertex_edge_span`'s `preferred_bucket` as "only used in recursion" (compact.rs:2512). The
  shared commit's tiling never receives the caller's placement preference, and the planner's own
  `positions` were already dead (the compiler warned `unused variable: positions` in the unified
  variant). So `preferred_bucket` / `preferred_extra` currently affect only *sizing*
  (`min_required += preferred_extra`), not *placement*. Next caller-facing fix: give
  `commit_vertex_edge_span_layout` (or the position helper it calls) the preference and pass `None`/0
  from the paths that have none, then re-check `compact_vertex_edge_span_one_step`, which passes
  `Some(bucket_index)` precisely to keep one bucket's slot indices stable.
  **Fixed (2026-09-20, `97c316f55`):** `commit_vertex_edge_span_layout` now takes
  `preferred_bucket` / `preferred_extra` and forwards them to
  `calculate_label_edge_span_positions_by_resident_slots`; the rebalance passes its caller's values and
  the two leaf-relocation call sites pass `None` / `0` as before. Clippy's "parameter is only used in
  recursion" signal on `rebalance_vertex_edge_span` is gone (0 occurrences) and the suite is 611/0 — the
  one-step compaction's slot-stability tests now hold by policy rather than by accident. Still
  outstanding from the same family: the planner computes and returns `positions` that no caller uses
  (dead work; drop it when the full delegation lands), and `preferred_extra` still only *sizes* the span
  for the paths that do not pass a bucket index.
  **Unified rewrite re-attempted on the new base (2026-09-20, saved `/tmp/unified_on_newbase_compact.rs`):**
  with the landed pieces (`bucket_resident_rows`, `fold_logs`, preference wiring) the rewrite's publish
  section (236 lines, all four inline branches) collapses into
  `materialize_labeled_vertex_edge_plan(leaf, &buckets, true, compact)` +
  `commit_vertex_edge_span_layout(...)` and the suite reaches **608/3** — the three failures
  (`compact_vertex_edge_span_shrinks_vertex_edge_span`, `incremental_vertex_edge_span_compact_preserves_*`,
  `labeled_rewrite_within_pinned_leaf_does_not_re*`) all report the same
  `Store(CollectAllocationOverflow)` and did **not** move when the planner's `total_live` switched from
  `Σ degree` to the shared resident definition (physical region + log), so their under-count is on a
  third path (candidates: `prepare_vertex_edge_span_for_overflow_log_fold`'s `stored_slots() + log`
  preflight, the leaf-pin growth retries, or the in-window layout the plan receives). Reverted to keep
  the tree green; the attempt is saved.
  **Insight from that attempt worth keeping:** on HEAD the planner's `Σ degree` budget is internally
  consistent with the *bulk* inline branch, which publishes live rows (`with_edge_range(row_start,
  bucket.degree())` + `-1` log head). The budget and the published width are two sides of one decision,
  so any unification must move them together — which is exactly what `fold_logs` now names.
  **Third path narrowed, and a real inconsistency found (2026-09-20):** the three failing rewrite tests
  did not move when the planner's `total_live` switched to the shared resident definition, and they did
  not move either when the *commit-side* position calculator stopped counting tiny residents
  (`calculate_label_edge_span_positions_by_resident_slots` used `stored_slots()`, which returns `degree`
  for tiny **after K=4**, while the plan-side calculator already uses the
  `bucket_resident_region` SSOT, i.e. 0 for tiny). That inconsistency is real and worth fixing on its own
  terms — ADR 0096 §5 says tiny buckets take a zero resident term in the position calculators — but it is
  not what fails these three. So the under-count is in a third place; ranked candidates left: the
  `prepare_vertex_edge_span_for_overflow_log_fold` preflight (`stored_slots() + log`),
  the leaf-pin growth retries, and the `in_window_layout` the plan receives. Next: one surgical print in
  that preflight plus one at the plan's `in_window_layout` reject, run only
  `compact_vertex_edge_span_shrinks_vertex_edge_span`, and do not sweep the crate with instrumentation.
  Attempts saved: `/tmp/unified_on_newbase_compact.rs` (608/3) and
  `/tmp/unified_on_newbase2_compact.rs` (608/3 with the tiny-resident change) — neither is green, so
  neither is landable; the tree is back at 611/0.
  **LANDED (2026-09-20, `8f137196d`): the rewrite has one publish path.** The three failures above were
  the tiny resident term after all — the first attempt kept `.max(bucket.degree())` outside the tiny
  branch, which re-introduced `degree`. The measured proof was
  `calculate_label_edge_span_positions_by_resident_slots: span_slots=80 < effective_live=81` from the
  existing `log_collect_overflow` hook (it is `#[cfg(target_family = "wasm")]`, so its non-wasm arm now
  prints too — that hook, not crate-wide instrumentation, is the cheap way to find these). With tiny
  zeroed outside the floor, the tree-root region used for trees and the planner budget on
  `bucket_resident_rows`, the four inline branches (236 lines) collapse into one
  `materialize` + `commit` call: **611/0**, net −147 lines, scoped fmt/clippy clean (no lib warnings).
  Focused canbench `compact` 4/4 improved (median −216.86 K instructions, 0 regressed) and `ins` 20
  benches 0 regressed; the largest insert delta (+1.45%) is inside canbench's noise threshold, and
  `bench_l_ins_*` per-bench deltas are ≤1.90% on ≈370-instruction micro benches, so a repeat run before
  treating it as a regression. Still open: delegation of `rebalance_vertex_edge_span` itself
  (`for_growth` vs `for_rebalance`), promotion rework, and unfiltered `canbench --persist`.
  **Delegation re-tried on the unified base (2026-09-20, saved `/tmp/delegated_on_unified_compact.rs`,
  597/14):** with the single publish path in place the shim is 115 lines shorter than the fourth
  implementation, but the failure set is the same shape as before, so the blockers are the two base
  resolution policies, not the publish mechanics: `resolve_labeled_edge_base_for_growth` returns
  `CollectAllocationOverflow` after four relocation attempts (the `mixed_label_hub_*` /
  `labeled_hub_33_labels_*` inserts and `batch_relocates_*` / `expanded_slab_*` see it) while
  `resolve_labeled_edge_base_for_rebalance` never fails — it relocates once and then falls back to
  `labeled_edge_base_from_first_bucket(src)`. **This is a policy decision, not a bug to guess at:**
  should an insert that needs a bigger span than its pinned leaf can host *fail*, or keep the current
  span and let the publisher report the shortfall? The fourth implementation chose "keep and continue"
  and the fixtures depend on it. Secondary blockers unchanged: `compact_vertex_edge_span_one_step`
  expects the log-preserving path (`OverflowRewrite` + stable slot indices), and four assertion tests
  (`labeled_relocate_commit_order` `left: 256`, `labeled_segment_relocate_releases_single_footprint`
  `left: 0`, `labeled_segment_slide_coalesces_adjacent_free` `left: 2`,
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` `hot_capacity > stored`) still encode the

  **Decision record needed — "may a span rewrite keep its base when the leaf cannot host the bigger
  span?"** Facts gathered 2026-09-20:

  * The plan's growth resolution has exactly one caller (`compact.rs:772`, inside
    `rewrite_vertex_edge_span_read_and_plan`); the fourth implementation's
    `resolve_labeled_edge_base_for_rebalance` has exactly one caller (`compact.rs:2420`). So the two
    policies are reachable only through those two entry points.
  * `resolve_labeled_edge_base_for_growth` order: in-leaf pin → (leaf pinned) up to four
    relocate-and-retry rounds → **error**; (leaf unpinned) pin once → in-leaf pin → **error**.
  * `resolve_labeled_edge_base_for_rebalance` order: in-leaf pin → (leaf pinned or relocate in
    progress) relocate once, set `LABELED_REBALANCE_LEAF_RELOCATED`, retry, then
    **`labeled_edge_base_from_first_bucket(src)`** — i.e. keep the current anchor — and the same
    fallback when the leaf is unpinned. It errors only for a vertex with no non-tiny bucket at all.
  * `tail_append_labeled_edge_base` is the relocate-in-progress escape: it grows
    `elem_capacity` by `new_alloc` and hands back the old capacity as the base.
  * Downstream, `commit_vertex_edge_span_layout` publishes `stored_slots = new_alloc`
    unconditionally. In the fallback case the span's *base* stays while its *cover* grows, so the
    cover can run past the leaf window and overlap a leaf mate — the GAP-005 overlap hazard, which is
    only `debug_assert`-guarded.
  * HEAD's green suite depends on the never-fail policy: `insert_edge_skip_leaf_cascade` skips the
    leaf *cascade* but still runs the vertex rebalance, so the hub fixtures (`mixed_label_hub_{20,33,50}`,
    `labeled_hub_33_labels_bounded_insert_time`, `batch_relocates_*`, `expanded_slab_*`) reach it.

  Options: **(A)** strict for everyone (delegation needs an explicit "fallback tolerated" argument and
  those fixtures must be re-derived, because on HEAD they pass only by keeping the base);
  **(B)** never-fail for everyone (smallest diff, but the insert path would accept an inflated cover —
  an invalid state by ADR 0096's own overlap rules); **(C, recommended)** strict for *required* growth,
  best-effort for *proactive slack* (`force_slack_grow && old_alloc >= min_required` downgrades to an
  in-place rewrite, already implemented in the delegated attempt), plus an explicit
  "maintenance may fall back" policy argument so the rebalance keeps its contract without hiding the
  difference inside one resolver. (C) needs: a policy parameter threaded from the rebalance,
  the three assertion-level tests re-read against that contract, and an ADR 0096 §5 row naming
  who may keep a base and who must obtain one.

  **The path that actually needs the never-fail policy (2026-09-20): overflow-log exhaustion.**
  `insert.rs:648` handles `Err(LaraOperationError::SegmentLogFull)` by calling
  `rebalance_edge_log_leaf_for_labeled(src, true, true)` and then re-reading the bucket and retrying —
  so the design's answer to "log is full" *is* a rebalance/relocation, exactly as ADR 0096 and the two
  insert tests say (`insert_beyond_initial_label_edge_span_capacity_relocates_labeled_leaf`;
  `labeled_insert_does_not_grow_elem_capacity_for_hub_growth`, which forbids tail-append for hub growth
  and requires block-aligned leaf allocation). `rebalance_edge_log_leaf_for_labeled` walks **every**
  vertex of the leaf with `grow_vertex_span = true, use_log_fold_prelude = true` and then calls
  `edges.release_log_segment(leaf)`, so the recovery is: fold each vertex's log into its span, grow the
  spans, release the leaf's log area, retry the insert. Per-leaf log capacity is real
  (`read_overflow_log_state(leaf) -> (used, capacity)`, `SegmentLogFull`,
  `overflow_log_same_leaf_second_run_capacity_exhaustion_rolls_back` for the batch path).
  Consequences for the decision: the never-fail fallback is what lets this leaf-wide pass tolerate a
  vertex whose span cannot grow — but the pass then *releases the log segment*, so a vertex whose fold did
  not complete would leave rows in a released area. That makes the fallback a **correctness** question,
  not a convenience: the policy must be "fold and grow, or fail closed", and if a fallback is kept it may
  only downgrade *proactive slack*, never a fold that still holds rows. Verify before deciding:
  whether `LogStore::release_segment` asserts/rejects a non-empty segment (`lara/edge/init.rs:285` →
  `log.release_segment`), and how the hub fixtures' `insert_edge_skip_leaf_cascade` inserts ever reached a
  state where the fold could not complete. Recommended shape (C''): required growth must be achieved by
  leaf relocation (never tail-append), slack stays best-effort, and an incomplete fold is a hard error;
  the shared resolver then needs an explicit policy argument that says *which* of those a caller accepts,
  instead of one resolver that is strict for some callers and forgiving for others.

  **Verified (2026-09-20): `release_segment` silently zeroes a non-empty log.**
  `LogStore::release_segment` (`lara/edge/log.rs`) and its inline-property twin
  (`lara/edge_inline_property/log.rs`) write zeroes over every entry up to the segment's `idx` and then
  reset the index to 0. There is **no emptiness check** — the only test nearby
  (`release_segment_clears_entries_and_index`) pins exactly the zeroing behaviour. So if any vertex's
  fold did not move its rows out of the log, `rebalance_edge_log_leaf_for_labeled`'s final
  `release_log_segment(leaf)` destroys them silently. That turns the resolver's never-fail fallback from
  a convenience into a **row-loss hazard**: the fallback can leave rows in the log precisely when the
  span could not grow, and the caller then releases the log area unconditionally.

  **Recommended direction (C''), for the user to confirm:**
  1. Fail closed at the owning layer first: `release_segment` must reject a non-empty segment (typed
     error, not zeroing), so no caller can lose rows by releasing a log it did not empty.
  2. `rebalance_edge_log_leaf_for_labeled` must propagate a failed fold *before* releasing, rather than
     relying on the resolver to swallow it.
  3. One resolver contract for every caller: required growth is achieved by leaf relocation (block
     aligned — `labeled_insert_does_not_grow_elem_capacity_for_hub_growth` already forbids tail-append);
     if the leaf cannot host it, error. Proactive slack stays best-effort, and a best-effort downgrade
     keeps the cover at `old_alloc` so a published cover is never larger than the owned region.
  4. Then the delegation of `rebalance_vertex_edge_span` is a pure refactor (both callers share the
     contract) and lands with the 115-line deletion.
  5. Tests: add a regression that a fold which cannot complete neither releases the log nor reports
     success; re-read the three assertion-level tests against the contract; the hub fixtures should then
     exercise real leaf relocation (they currently pass through the fallback, which is the state this

  **Essential design (proposed contract formulation, 2026-09-20 — for confirmation before it moves into
  ADR 0096 §5 / `design/storage/lara.md`).** Everything found this session follows from two invariants
  that the current code states nowhere:

  * **I1 — a cover is a claim that must be backed.** For every vertex, `[base, base + stored_slots)`
    must lie inside the physical region the store actually owns for it, and that region must lie inside
    the leaf window. Publishing `stored_slots = new_alloc` while keeping the old base (the never-fail
    fallback) violates I1: the cover can overlap a leaf mate, which is the GAP-005 hazard. Corollary:
    growth must be *backed before published* — "decide, then publish, then back" is not available.
  * **I2 — a log is unmaterialized content, and releasing log space is destroying content.** A bucket's
    rows live in `prefix ∪ log`; a log entry exists only until a rewrite writes it into a span. Therefore
    clearing a log head requires that every one of its rows was written, and *releasing a leaf log segment
    requires the whole leaf's logs to be empty*. `release_segment` currently zeroes without checking, so
    this invariant is unenforced and the never-fail fallback can reach it with rows still in the log.

  Consequences that make the rest mechanical (and that the landed work already starts):

  1. **Ownership has one accessor.** How many slots back a bucket = `bucket_resident_region` /
     `bucket_resident_rows` (tiny 0, slab prefix, tree root region, plus log). `stored_slots()` is a wire
     field with mode-specific meaning (tiny: used width; tree: logical extent) and must never size
     anything. Landed: `729214852` (one definition) and `8f137196d` (planner uses it).
  2. **One writer per decision.** Sizing and placement come from one snapshot and one definition; the
     commit owns positions. Landed: `8f137196d` (single publish path), `97c316f55` (preference reaches
     the tiler).
  3. **Growth is leaf-level.** A pinned vertex cannot grow past its leaf's room; the leaf block is the
     backing region and its vertices are tiled together. So required growth = relocate/re-tile the leaf
     (block aligned); tail-append is only valid when the leaf is unpinned or a relocation is in flight —
     which is exactly what `labeled_insert_does_not_grow_elem_capacity_for_hub_growth` already asserts.
  4. **Slack is optional; growth is not.** Callers differ in how much *spare* they ask for, never in
     whether the content fits. So there is no need for two resolvers: one resolver, with `required`
     (must fit: relocate the leaf, else error) versus `slack` (best effort: downgrade to the current
     span, keeping the cover equal to the owned region).
  5. **Fail closed, never degrade.** A degradation that keeps an invariant would be local; the ones here
     are not — an unbacked cover corrupts a neighbour's ownership, an undrained release destroys rows
     elsewhere in the leaf. Every such case must be an error the caller can act on (relocate, fold,
     retry), which is the shape the log-full recovery already has: fold every vertex, verify empty,
     release, retry.
  6. **Transaction shape.** The batch path already has the right form (`reserve_one_orientation_batch` /
     `BatchReservation`: validate → reserve → commit → rollback). A span rewrite should have the same
     shape: snapshot → size → reserve/place → write content → publish descriptors and vertex → clear the
     logs that were drained; anything before the first canonical write is abortable, anything after it
     must be infallible. That is what the deletion in `8f137196d` moved toward.
  **Landed (2026-09-20, `553a72f09`): I2 is enforced.** `release_leaf_overflow_log` drains-checks the whole
  leaf before calling the store, all four release sites (leaf relocation, reclaim, the log-full recovery and
  the sole-active-vertex fast path) go through it, and the store-level release is no longer reachable with
  rows still chained. New regression
  `releasing_a_leaf_overflow_log_with_undrained_rows_is_refused` stages a log-backed bucket and proves the
  refusal; suite 612/0, scoped fmt/clippy clean. I2's ADR text is in ADR 0096 §5. Remaining from the
  essential design: **I1** (the commit must prove `cover ≤ owned region` before publishing, so a best-effort
  downgrade keeps `old_alloc`), the single `required` / `slack` resolver, and then the delegation of
  `rebalance_vertex_edge_span` (115 lines) plus the three assertion-level tests.
  **I1 enforcement attempted and corrected (2026-09-20).** Checking `new_base + new_alloc` against
  `labeled_leaf_physical_range(src)` (the pinned leaf's block) is **wrong**: 46 tests fail with
  `SpanCoverExceedsRegion`, including every relocation/leaf-pin fixture and the plain
  `labeled_insert_does_not_grow_elem_capacity_for_hub_growth` insert. The leaf's physical block is not the
  backing of a vertex's *edge* span; a span may sit in free-span space outside any leaf block, and the block
  is only the region a relocation tiles into. So the corrected formulation of I1 is:
  **a published cover must equal the span the base resolution actually obtained.** The backing is the
  reservation itself (`old_alloc` when the resolver kept the base, `new_alloc` when it placed or
  tail-appended one), which means the resolver must return *what it obtained*, not the caller's request —
  `resolve_*` should yield `(base, width)` and the plan/commit must publish that width. HEAD's fourth
  implementation violates this in exactly one place: `resolve_labeled_edge_base_for_rebalance` falls back to
  `labeled_edge_base_from_first_bucket` while the caller keeps publishing `new_alloc`. That is the phantom
  cover, and it is fixed by publishing the obtained width rather than by a commit-time region check.
  **Landed (2026-09-20, `4296dbab3`): I1 is enforced by publishing the obtained width.** Both resolvers now
  return `(base, width)`; `for_rebalance`'s fallback reports `self.vertices.get(src).stored_slots`, so a kept
  base can no longer be widened into a phantom cover, and `for_growth` reports the request it placed,
  relocated or tail-appended. Suite 613/0 with the new regression
  `base_resolution_never_reports_a_width_it_did_not_obtain`, which also *corrected the invariant's wording*:
  the fallback width is not necessarily the pre-call cover — a relocation may have grown the span — so the
  check is `width <= the vertex's backing span`, which is what a commit-time region test could not express.
  Remaining from the essential design: the single `required` / `slack` resolver (they still differ in
  whether a missing span is fatal), then the delegation of `rebalance_vertex_edge_span` (115 lines) and the
  three assertion-level tests that encode the old placement.
  **Resolver unification attempted and reverted (2026-09-20, saved
  `/tmp/unified_resolver_attempt_compact.rs`).** The attempt introduced `SpanResolutionPolicy { Required,
  SlackMayBeDropped }`, one `resolve_labeled_edge_base` plus a policy tail
  (`tail_append` for a required span while a relocation is in flight, the current cover for slack, an error
  otherwise), and kept the two public wrappers with their existing signatures. It compiles and 613 green
  tests pass, but `compaction_cursor_packs_alternating_tombstones_across_steps` **overflows the stack**:
  the restructured retry/relocation order recurses where the two original bodies did not (the original
  growth body retried in-leaf *before* each relocation and never took the rebalance resolve guard; the
  original rebalance body relocated once with that guard held). Two other behaviour details also drifted in
  the attempt: the `SlackMayBeDropped` path stopped relocating when the leaf was already pinned, and
  `LABELED_REBALANCE_LEAF_RELOCATED` was only set for that policy. Next attempt should keep each original
  body's control flow *verbatim* and factor only the policy-dependent tail; do not restructure the retry
  loop while doing it. The tree is back at 613/0 with the two resolvers as they are.
  **Policy tail factored instead (2026-09-20, `d994fc500`).** Rather than restructuring the retry loops
  (which is what recursed), the policy-dependent tail is now shared: `finish_span_resolution(src,
  new_alloc, policy, relocation_in_progress)` holds the three outcomes (tail-append for a required span
  while a relocation is in flight, an error for a required span, the current span for slack) and each
  resolver keeps its original control flow verbatim. 613/0, zero behaviour change. Remaining work for the
  delegation, in order, with no restructuring: (1) add a policy-dispatching resolver
  (`Required → resolve_labeled_edge_base_for_growth`, `SlackMayBeDropped →
  resolve_labeled_edge_base_for_rebalance`); (2) rename the current `rewrite_vertex_edge_span` to
  `..._with_policy(..., policy)` and keep a thin `Required` wrapper so its ten call sites stay untouched;
  (3) thread that policy into `rewrite_vertex_edge_span_read_and_plan`, where the base resolution happens;
  (4) replace `rebalance_vertex_edge_span`'s body with a call to the internal rewrite using
  `SlackMayBeDropped` (115 lines); (5) re-read the three assertion-level tests, whose expectations encode
  the old placement. Expect the hub fixtures to fail *honestly* at the tiling
  (`span < effective_live`) if the leaf relocation cannot host the span — that is the policy decision
  recorded above, now expressed in code rather than inferred.

  **"Relocation cannot host it" is usually NOT a memory-growth failure (verified 2026-09-20).** Three
  distinct classes reach the policy tail:

  * **(A) leaf-window sizing.** `plan_labeled_leaf_relocation` computes
    `raw_len = resident_geometry + active_vertices + max(resident_geometry/8, seg)` (then rounds up to a
    block), where `resident_geometry` is the sum of the leaf's *resident* rows (physical region + log).
    It never consults the requesting vertex's `new_alloc`. So a vertex asking for more than its resident
    share — proactive slack, or a required span wider than its rows — fails to place
    (`try_labeled_vertex_edge_base_in_pinned_leaf` returns `None`) **with memory perfectly available**.
    This is the class the hub fixtures hit: in the delegated run they failed with
    `CollectAllocationOverflow` *after* the relocation rounds, i.e. relocations themselves were fine.
  * **(B) reservation failure.** Only here is growth the question: `GrowFailed` from wasm-memory growth,
    `AllocSpaceCapReached`, or the index-space overflow in `tail_append_labeled_edge_base`'s
    `checked_add_slot_exclusive_end`.
  * **(C) structural.** No non-tiny bucket at all, so `labeled_edge_base_from_first_bucket` itself errors.

  Design consequence: (A) is fixable *honestly* by making the relocation request-driven — size the new
  leaf block from `max(resident_geometry, floors requested by the vertices being grown)` instead of
  resident geometry alone. Then a required span succeeds whenever memory allows, the never-fail fallback
  stops being load-bearing, and the policy question narrows to (B) alone. Next verification before
  implementing: print `old_len`, `new_len` from `plan_labeled_leaf_relocation` and the requested
  `new_alloc` for `mixed_label_hub_20_labels_500_edges_each` — if `new_len - old_len` is large and the
  placement still fails, (A) is confirmed with numbers rather than by construction.

  **(C) in detail — "no base to fall back to", and it is reachable (2026-09-20).** The slack tail reports
  the span a vertex currently holds, and the vertex's *content* base is not stored: ADR 0096 §5 derives it as
  the first **non-tiny** bucket's `edge_start` (`labeled_edge_base_from_first_bucket`), because tiny anchors
  are placeholders and releasing from one poisons the free-span store with live ranges. So a vertex with no
  buckets, or only tiny ones, has no base to report and that function errors with
  `CollectAllocationOverflow` — no allocation is attempted, so this class has nothing to do with memory.

  Reachability, verified in code: `next_vertex_edge_span_allocation(0, min_required = 0, segment_size)`
  returns `min_required.max(segment_size)` = `segment_size`, so a *spanless* vertex (all buckets tiny,
  degree > 0) that is asked for **slack** requests a real span. `rebalance_edge_log_vertex_for_labeled`
  guards only `degree() == 0 || is_default_edge_labeled()`, so an all-tiny vertex passes that guard, and the
  leaf-wide log-full recovery runs every vertex of the leaf. If the leaf cannot place the requested span, the
  slack tail then has no base to report and the whole recovery fails — on HEAD too, not only in the
  delegation. Two cheap, independent fixes: (a) do not request slack when the resident sum is zero (keep
  `new_alloc = 0`, so the resolver is never asked and (C) becomes unreachable); (b) make the non-wasm
  `log_collect_overflow` observable in tests — it is a no-op today, which is why this class prints nothing
  where it fails (this session's decisive `span_slots=80 < effective_live=81` came from temporarily

  **(A) confirmed with numbers (2026-09-20).** Instrumenting `plan_labeled_leaf_relocation` and the
  rebalance's request for `mixed_label_hub_20_labels_500_edges_each` gives a repeating pattern:

  ```
  TMPREQ   old_alloc=10400 min_required=9412 new_alloc=11700 leaf_max=Some(10400)
  TMPRELOC old_len=10400 new_len=10592 resident_geometry=9412 active=1 block_len=16
  TMPREQ   old_alloc=10592 min_required=9586 new_alloc=11916 leaf_max=Some(10592)
  TMPRELOC old_len=10592 new_len=10800 resident_geometry=9586 ...
  ```

  The content already fits (`min_required` 9412 < `old_alloc` 10400); what fails is the *proactive slack*
  (`new_alloc` 11700). The relocation sizes the new block as
  `resident_geometry + active_vertices + max(resident_geometry/8, seg)` = 9412 + 1 + 1179 = **10592**, i.e.
  ~2 % above the current block and never the request, so `leaf_max` stays below `new_alloc`, the retry loop
  ratchets by ~200 slots a round and never converges. Fix: the requesting vertex's share in the relocation's
  sizing must be `max(resident, requested_alloc)` — `plan_labeled_leaf_relocation` knows `src`, so it needs
  only the request passed in (`relocate_labeled_leaf_physical_block` gains a floor-carrying variant; the
  plain entry point keeps passing 0).
  **(A) attempted, half of it landed as diagnosis (2026-09-20, saved
  `/tmp/classA_classC_attempt_{compact,batch_write}.rs`).** Threading the requesting vertex's span into
  `plan_labeled_leaf_relocation` (its share becomes `max(resident, requested_floor)`) worked for the 20- and
  33-label hubs but left `mixed_label_hub_50_labels_1000_edges_each` failing: the *re-tiling* also assigns
  each vertex only its resident share (`rebalance_labeled_leaf_weighted_slide_in_block` computes `span_slots`
  from residents), so a bigger block alone does not place the request. (A) therefore needs both halves — the
  block sizing *and* the per-vertex span assignment during the slide — before it converges. Reverted to keep
  the tree green; the sizing half is preserved in the saved files.
  **(C) fixed and landed (`f6c2acaef`):** tiny contributes zero resident in both sizing paths, a spanless
  vertex short-circuits in the rebalance, and the plan skips slack when `total_live` is zero. The regression
  `spanless_tiny_vertex_does_not_request_slack` *found* the divergence, and the existing
  `log_collect_overflow` hook — temporarily made visible for non-wasm — named it exactly:
  "resolve_labeled_edge_base_for_rebalance: leaf not pinned; pinning before allocating" then
  "labeled_edge_base_from_first_bucket: src=VertexId(0) has no buckets". Making that hook observable in tests
  under a cfg would have saved the instrumentation round; worth doing if class C recurs.
  **(A) exact remaining edits (2026-09-20, partial state in `/tmp/classA_partial_{compact,batch_write}.rs`,
  superseded by the earlier `/tmp/classA_classC_attempt_*`).** Both halves are known; only the edit order
  needs care:

  1. `plan_labeled_leaf_relocation(&self, src, requested_floor: u32)` — in its vertex loop, after the bucket
     fold produces `resident_slots`, add `if u32::from(vid_u) == u32::from(src) { resident_slots =
     resident_slots.max(requested_floor); }` before the `resident_geometry` accumulation. Both
     `batch_write.rs` call sites then pass `, 0`.
  2. `relocate_labeled_leaf_physical_block(src)` stays as a wrapper passing floor 0; the existing body is
     renamed `relocate_labeled_leaf_physical_block_with_floor(src, requested_floor)` and forwards the floor to
     `plan_labeled_leaf_relocation(src, requested_floor)`.
  3. Inside that body, its slide call passes the floor:
     `self.rebalance_labeled_leaf_weighted_slide_in_block(src, leaf_start, leaf_len, false, true, None)`
     becomes `(src, requested_floor, leaf_start, leaf_len, false, true, None)` — note the arguments are on one
     line, which is why the anchor-based patch missed it; the second call site (the fresh-block path, around
     the `labeled_leaf_weighted_slide_commit` bench scope) has `src, new_start, new_len, true, true, None` and
     gains `requested_floor` the same way.
  4. The two resolvers' five `relocate_labeled_leaf_physical_block(src)?` calls become
     `..._with_floor(src, new_alloc)?`.
  5. The slide's own signature gains `requested_floor: u32` after `src`, and in its `slices` loop the
     per-vertex `resident` becomes `resident.max(requested_floor)` for `vid == src` before it is added to
     `total_resident` (so the extra room comes out of the gaps).

  Expected outcome: `mixed_label_hub_50_labels_1000_edges_each` converges instead of ratcheting, required
  growth succeeds whenever memory allows, and the never-fail fallback stops being load-bearing for the hub
  fixtures.

  **(A) implemented in full, one fixture left (2026-09-20, saved `/tmp/classA_full_{compact,batch_write}.rs`
  → 613 passed / 1 failed).** All five edits from the list above are applied and compile: block sizing honours
  the request, the relocation entry point carries the floor (the plain wrapper and the plain slide caller
  `rebalance_labeled_leaf_weighted_slide` pass 0), the resolvers drive relocation with the span they are
  placing, and the slide's per-vertex `resident` becomes `max(resident, requested_floor)` for the requesting
  vertex. `mixed_label_hub_{20,33}` and their variants converge; only
  `mixed_label_hub_50_labels_1000_edges_each` still fails (`label_idx=12 edge_i=98`,
  `CollectAllocationOverflow`, ≈73 s to run). Instrumentation results for that residue:
  the existing `log_collect_overflow` hook stays **silent**, so the error comes from a plain
  `ok_or`/`map_err` construction site rather than a logged one; instrumenting compact.rs's 66 such sites hit a
  type error at its line-190 site (that site needs the `ok_or_else(|| ….into())` form), so the sweep needs that
  fix before it can name the residue. Next session, in order: (a) fix that instrumentation form and name the
  site; or (b) bisect the two halves on that fixture (remove the slide half, then the sizing half) to see which
  one the residue depends on. Everything else from this session is green at 614/0.
  **Instrumentation for that residue is a dead end (2026-09-20, second attempt).** The 66-site sweep of
  compact.rs compiles except for the line-190 site, which needs the `.into()` form; teaching the sweep that
  form produced eleven classes of syntax errors ("expected expression, found `let`", "expected `;`, found
  `if`", …), i.e. the regex keeps matching `ok_or(...)` inside larger expressions and dropping the punctuation
  the surrounding expression needs. Do **not** retry regex instrumentation of construction sites: it has now
  broken the tree twice (the earlier crate-wide sweep touched a foreign file). Method that worked instead, in
  order of preference: (1) the existing `log_collect_overflow` hook, which named class C's site in one run but
  is silent here, meaning the residue comes from a plain `ok_or`/`map_err`; (2) **bisection on the two class-A
  halves** — restore `/tmp/classA_full_compact.rs`, remove the slide half (step 5) and run
  `mixed_label_hub_50_labels_1000_edges_each` (≈73 s), then remove the sizing half (steps 1–2) as well and run
  again; whichever half's removal changes the failure names the residue's dependency; (3) if a site must be
  identified directly, wrap only the *few* sites on the path (`rgba`-free: read the failing function's
  `ok_or` lines and edit them by hand). Tree is back at 614/0 with class A reverted.
  **The numbers retired class A and answered the whole question (2026-09-20).** With both halves applied, two
  prints on `mixed_label_hub_50_labels_1000_edges_each` show runaway growth:

  ```
  TMPRELOC  old_len=1_053_766_992 new_len=1_333_673_856 resident_geometry=1_185_487_866 active=1 floor=1_185_487_866
  TMPSLIDE  vid=0 resident_after_floor=1_185_487_866 floor=1_185_487_866 leaf_len=1_333_673_856
  TMPRELOC  old_len=1_333_673_856 new_len=1_687_930_976 ...            (×1.26 every round, seven rounds)
  ```

  Two conclusions, both structural:

  1. **Feeding the allocation request back into "resident" is a positive feedback loop.** The floor I passed
     was `new_alloc` (content + proactive slack, ≈ `old_alloc × 1.125`); the slide then assigns the requesting
     vertex that many slots, its cover grows, and the *next* round's `resident_geometry` (which sums covers)
     is bigger again — exponential until the index space is exceeded and a plain `ok_or` reports
     `CollectAllocationOverflow`. Class A as I specified it cannot work: a span *assignment* may only be sized
     from content (`min_required`), never from the request.
  2. **The hub fixtures never needed growth.** In the earlier measurement the content already fitted
     (`min_required` 9412 < `old_alloc` 10400); only the proactive slack (11700) did not. Since the leaf block
     already sums every vertex's resident rows, the block was never too small — the *slack* request was simply
     being treated as required by the insert path. The correct fix is therefore the policy one: make proactive
     slack best-effort (drop it and keep the current span when it cannot be placed), which is exactly the
     `SlackMayBeDropped` tail that already exists, rather than growing leaves. That also removes the last
     reason the never-fail fallback looked load-bearing.

  So: class A is not a growth-sizing problem at all; it is the slack policy applied to the insert path. The
  one-line shape is the plan's `new_alloc` expression — when the content fits (`old_alloc >= min_required`) and
  growth cannot be placed, keep `old_alloc` instead of failing — i.e. `force_slack_grow` downgrades to an
  in-place rewrite. Tree back at 614/0; `/tmp/classA_full_*` is superseded.
  **Landed (2026-09-20, `db04af585`): spare capacity is a hint, never a reason to allocate.** Both sizing
  paths now check `force_slack_grow && old_alloc >= min_required && try_labeled_vertex_edge_base_in_pinned_leaf(src,
  new_alloc).is_none()` and keep `old_alloc` in that case, so the wider span is taken only when the current
  window already hosts it: no resolution, no relocation, no failure for spare room, and content-fitting
  rewrites stay in place. 614 passed / 0 failed, scoped fmt/clippy clean. This also supersedes class A
  entirely (no growth machinery for slack). Still to add when the two sizing sites collapse into one with the
  delegation: a regression that a slack request never changes the leaf's physical block length (a single site
  makes that a precise unit assertion; today the observable is spread over two paths).
  **Delegation reaches 611/3 with policy threading (2026-09-20, saved
  `/tmp/delegation_policy_611_3_compact.rs`; the earlier policy-less shim is
  `/tmp/delegation_nopolicy_compact.rs` at 604/10).** The shim itself (−152 lines) was never the
  blocker: what the hub fixtures needed was the maintenance *policy*, and once the rewrite carries a policy
  (`resolve_labeled_edge_base_for_policy`, `rewrite_vertex_edge_span_with_policy`, planner parameter) and the
  shim passes `SlackMayBeDropped`, every hub fixture passes (604/10 → 611/3). The three residuals are precise:

  * `overflow_rewrite_compacts_only_log_suffix_before_slab_tombstones` and
    `edge_overflow_compaction_does_not_fold_inline_property_bytes_log` — both drive
    `compact_vertex_edge_span_one_step`, which folds one bucket's log itself and asserts unchanged slot
    indices. The fourth implementation *always* preserved logs in its publish (`fold_logs = false`) and folded
    through a separate prelude (`use_log_fold_prelude`) before calling the rebalance. Passing
    `fold_logs = false` from the shim alone is **not** enough: it regressed to 598/16, because the log-full
    recovery relies on that prelude's fold actually happening. The delegated path must reproduce
    "prelude folds, publish preserves": keep `fold_logs = false` *and* run the fold prelude where the fourth
    implementation did, instead of folding in the publish.
  * `vertex_edge_span_rewrite_weights_slack_by_label_degree` asserts that a hot label receives more slack
    (`hot_capacity > stored`). With the new rule that slack is only taken when the current window hosts it,
    the fixture apparently cannot place the slack at all, so the expectation encodes the previous
    slack-always-grown behaviour and needs re-deriving *as a distribution test* (assert the weighting when
    slack is granted) rather than weakening it.
  **Why `fold_logs = false` alone regressed (2026-09-20, diagnosed).** The fold is a two-step
  operation: `prepare_vertex_edge_span_for_overflow_log_fold` *sizes* the span (it grows the cover and
  returns — it does not write the log rows), and the caller's subsequent loop writes the log entries into the
  slab and clears the head. `rebalance_vertex_edge_span` is called *inside* the prepare step, before the fold,
  which is why the fourth implementation never folded in its publish (`fold_logs = false` is correct for the
  rebalance) and why threading `false` alone cannot be wrong for that reason — but it still regressed to
  598/16 because of **tiling width**: the fourth implementation computed positions from
  `stored_slots().max(degree)`, i.e. it reserved room for the *live* rows (prefix + log) even though it
  published only the prefix, while the shared commit now derives the tiling width from
  `bucket_resident_region` (the prefix alone) when `fold_logs = false`. The fold therefore wrote its rows past
  the room the tiling had reserved. Fix for the delegated path: decouple "reserved for tiling" from "published
  width" — with `fold_logs = false` the tiling must reserve `bucket_resident_rows` (prefix + log, the SSOT this
  session introduced) while the published `stored` stays the materialized run (the prefix, keeping
  `overflow_log_head`). The vertex cover (`new_alloc`) is already sized from the resident rows, so this stays
  I1-compliant.
  **Handover state for the delegation (2026-09-20, end of session).** Verified by running each state:

  * `/tmp/delegation_nopolicy_compact.rs` — shim only, no policy threading: **604/10**.
  * the policy-threaded state (shim passes `SlackMayBeDropped`, `resolve_labeled_edge_base_for_policy`,
    planner parameter, `rewrite_vertex_edge_span_with_policy` + `Required` wrapper): **611/3** — this is the
    state to reconstruct; it was **not saved under a correct name** (the file called
    `/tmp/delegation_policy_611_3_compact.rs` is actually the later, regressed one, now renamed
    `/tmp/delegation_foldthread_598_16_misnamed_compact.rs`). Reconstruction is mechanical: re-apply the four
    policy edits listed above to HEAD, and do *not* thread `fold_logs` yet.
  * `/tmp/delegation_foldthread_tiling_598_16_compact.rs` — policy threading **plus** `fold_logs = false` for
    the shim plus the tiling-width separation (tiling reserves `bucket_resident_rows` while the published width
    stays the materialized prefix): **598/16**, i.e. the tiling fix did not recover the fold-preserving
    behaviour either. So the two one-step-compaction failures need a different treatment than a flag and a
    wider tiling: compare `compact_vertex_edge_span_one_step`'s expectations against what the shared commit
    does for a log-backed bucket whose span already holds the content (the fourth implementation re-tiled from
    `stored_slots().max(degree)` and published `stored_slots_raw()`; `EdgeMoved { old_slot_index: 1,
    new_slot_index: 0 }` says the commit moved a log row into slot 0), i.e. read that path rather than adding
    another parameter.

  After reconstructing 611/3, the remaining three failures are: those two one-step-compaction expectations and
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` (re-derive as a distribution test, since slack is
  now only taken when the window hosts it). Tree is at 614/0.
  **Reconstructed and saved correctly (2026-09-20):** `/tmp/delegation_policy_v2_compact.rs` is the verified
  **611/3** state — delegation shim (−152 lines) + policy threading (`resolve_labeled_edge_base_for_policy`,
  planner parameter, `rewrite_vertex_edge_span_with_policy` with the public `Required` wrapper, and the shim
  passing `SlackMayBeDropped`), with no `fold_logs` threading. HEAD stays 614/0 with the delegation reverted;
  the file restores it in one `cp`. `head_before_reconstruct.rs` in the same directory is plain HEAD for
  diffing. Remaining three failures, in the order worth attacking: the two
  `compact_vertex_edge_span_one_step` expectations (read what the shared commit does for a log-backed bucket
  whose span already holds its content — the observed `EdgeMoved { old_slot_index: 1, new_slot_index: 0 }`
  says a log row was written into slot 0, i.e. the published run folded the log even though the one-step path
  folds it itself and asserts stable indices) and
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` (read the fixture's expectation: with slack only
  taken when the window hosts it, `hot_capacity > stored` may no longer be the right property).
  **Root cause of the two one-step failures, from reading the failures (2026-09-20).** Applying
  `fold_logs = false` from the *shim* (i.e. to every rebalance call) breaks ten rebalance tests with
  `CollectAllocationOverflow`, including `labeled_leaf_rebalance_folds_overflow_log` — so the rebalance itself
  *must* fold in its normal use; the flag was in the wrong place. What the one-step path needs is the opposite:
  `compact_vertex_edge_span_one_step` calls the rebalance with `preferred_extra = log_len` to make room, then
  folds that one bucket itself and asserts its slot indices are unchanged (`slab_survivor.edge_slot_index_raw()
  == 1`). The delegated rebalance folds the log *during its publish*, so by the time the one-step path inspects
  the result the row has already moved into slot 0 (`EdgeMoved { old_slot_index: 1, new_slot_index: 0 }`).
  Correct placement of the log policy is therefore **per caller of the rebalance**, not per shim: give
  `rebalance_vertex_edge_span` a `fold_logs` argument, pass `false` from `compact_vertex_edge_span_one_step`
  (and the fold-planned callers that fold themselves) and `true` everywhere else, then forward it to
  `rewrite_vertex_edge_span_with_policy`. The tiling-width separation (`bucket_resident_rows` for the reserved
  span, materialized prefix for the published width) is still required for that path and is preserved in
  `/tmp/delegation_foldthread_tiling_598_16_compact.rs`.
  **Per-caller log policy landed the fix (2026-09-20): delegation now at 612/2, saved as
  `/tmp/delegation_foldpercall_612_2_{compact,values,batch_write}.rs`.** `rebalance_vertex_edge_span` takes a
  `fold_logs` argument; the two `Some(bucket_index), log_len` callers (stepped compaction and the fold-planned
  path) pass `false` and everything else (maintenance, values, batch, tests) passes `true`; the rewrite pair
  forwards it to the commit, and the tiling reserves `bucket_resident_rows` (prefix + log) for the
  non-folding case while the published width stays the materialized prefix. That fixed
  `edge_overflow_compaction_does_not_fold_inline_property_bytes_log` and every other rebalance test, taking the
  delegated suite from 611/3 to 612/2. The two residuals:

  * `overflow_rewrite_compacts_only_log_suffix_before_slab_tombstones` now fails with
    `Option::unwrap()` on `None` at compact.rs:5231 — a lookup in the test (bucket slot or edge) returns
    nothing, so the remaining difference is a *position* the tiling chose, not the fold;
  * `vertex_edge_span_rewrite_weights_slack_by_label_degree` (`hot_capacity > stored`) — re-derive as a
    distribution test, since slack is only taken when the window hosts it.

  Restore with the three `cp`s above; HEAD stays 614/0.
  **Do not land the 612/2 artifacts yet: the residual is a visibility loss, not an expectation
  (2026-09-20).** The `None` in `overflow_rewrite_compacts_only_log_suffix_before_slab_tombstones` is
  `.find(|edge| edge.target == 1).unwrap()` on `iter_edges_for_label(hub, road)` — the edge the test expects to
  remain visible in the slab is **absent from the scan** after the stepped compaction, whereas the previous
  (folding) behaviour merely moved it (`EdgeMoved { old_slot_index: 1, new_slot_index: 0 }`). So the
  non-folding publish (`fold_logs = false` + prefix-only materialization + preserved `overflow_log_head` +
  the wider tiling reservation) leaves a row unreachable: either the fold that follows overwrites it, or the
  head is cleared while the row was never written. That is the same class as the `release_segment` hazard (I2)
  and must be explained before any of `/tmp/delegation_foldpercall_612_2_*` is landed — otherwise the
  delegation would convert a moved row into a missing row. Next step is a *targeted* read of
  `compact_vertex_edge_span_one_step`'s own fold (what it writes, where, and whether it clears the head) plus
  the commit's non-folding publish for the same bucket, not another parameter. HEAD stays 614/0; the 612/2
  artifacts are for that read's starting point only.
  **Where to look for that visibility loss (2026-09-20, from reading the stepped path).**
  `compact_vertex_edge_span_one_step` does not fold the bucket itself: for the log-backed case it calls the
  rebalance (now with `fold_logs = false`) and then **recurses**; the fold happens on the next pass, in the
  vertex-wide branch `if vertex.stored_slots > total_live && (any bucket has a head || stored != degree) {
  self.rewrite_vertex_edge_span(vid, None, 0, true /*compact*/, false, None) }`. So the row that disappears is
  lost between those two calls: step 1 publishes the prefix with the head preserved and (with `fold_logs =
  false`) a tiling that reserves prefix + log, and step 2 is a compacting rewrite that packs live rows and
  clears heads. Two concrete things to print in that test, one at a time, rather than adding parameters:
  (a) after step 1, the bucket's `stored_slots_raw()`, `degree()`, `overflow_log_head()` and the target's
  `edge_slot_index_raw()` from the log chain read; (b) after step 2, the same four, plus whether
  `iter_edges_for_label` sees the target before and after each step. That distinguishes "step 1 published the
  head without the rows" from "step 2 packed and dropped a row". Restore the 612/2 artifacts with the three
  `cp`s; HEAD is 614/0.
  **Mechanism of the visibility loss, measured (2026-09-20).** With the per-caller `fold_logs = false`, a probe
  after step ① prints `TMPSTEP1 slots=[(7, 6, 2)] seen=6` — i.e. the bucket has 7 prefix slots, 6 live rows, a
  log chain at head 2, and the scan sees 6 rows — and step ② never runs (no `TMPSTEP2`), so the compacting
  rewrite is not involved. The row is lost afterwards in the recursion's *per-bucket* step, which packs each
  label row (`stored_slots = degree`; the code comments say so: "Per-bucket steps may already pack each label
  row (`stored_slots == degree`) while the vertex-wide VertexEdgeSpan width stays oversized"). Packing to
  `degree` while a log chain is still present truncates the row that lives at ordinal `stored + offset` and has
  not been folded yet. In other words the stepped path *assumes the fold already happened* before it packs —
  which the fourth implementation's publish effectively provided, and the unified publish does not when
  `fold_logs = false`.

  Conclusion for the delegation: the per-caller flag is not the answer, and the stepped path's expectation
  ("`OverflowRewrite` with unchanged slot indices") is incompatible with a publish that owns the fold. The
  honest resolution is to let the publish own the fold (i.e. the 611/3 state, `fold_logs = true`), and
  re-derive `overflow_rewrite_compacts_only_log_suffix_before_slab_tombstones` — the observed behaviour there is
  `EdgeMoved { old_slot_index: 1, new_slot_index: 0 }` and the row remains visible — documenting in ADR 0096 §5
  that the fold is publish-owned, exactly as the log-ownership row already says for the rewrite path. The
  `fold_logs=false` machinery (`/tmp/delegation_foldpercall_612_2_*`) is therefore superseded and must not be
  landed as-is. HEAD is 614/0.
  **Stepped-test re-derivation plan (2026-09-20, session close).** The enum settles the intent:
  `OverflowRewrite(Vec<EdgeSlotMove>)` is documented as "one overflow suffix was folded; `moves` lists
  legacy-tombstone slot rewrites", `EdgeMoved(EdgeSlotMove)` as "one live edge was relocated inside its label
  bucket". With the publish owning the fold, the step's first call folds the suffix during the rewrite and then
  the step reports the *compaction* move, so the test should assert `EdgeMoved` with
  `old_slot_index: 1, new_slot_index: 0` and that the survivor (`target == 1`) is still visible — at slot 0.
  The fixture builds it as: `inline_property_test_graph()` + a hub, four inserted edges, `promote_bucket_to_slab`,
  `rebalance_edge_log_leaf_for_labeled(hub, true, true)`, remove `target == 0`, then insert `target` 100.. until
  the bucket has a log chain of length ≥ 3, remove the first log target with `remove_edge_matching_with_move`
  (asserting its moves are non-empty), and finally call `compact_vertex_edge_span_one_step(hub, 0, 0, insertion)`.
  The assertions after the survivor check (a second `compact_vertex_edge_span_one_step` call and its
  expectations) must be re-derived in the same pass — they were not read. State to start from:
  `cp /tmp/delegation_policy_v2_compact.rs crates/ic-stable-lara/src/labeled/graph/compact.rs` (611/3), then edit
  that test, then `vertex_edge_span_rewrite_weights_slack_by_label_degree` as a distribution property. HEAD is
  614/0 and everything from this session is committed.
  **Delegation at 613/1, saved as `/tmp/delegation_613_1_compact.rs` (2026-09-20, session close).** Both stepped
  tests are re-derived and pass — the publish owning the fold means the first step folds the suffix and then
  reports the compaction move it performs, so `overflow_rewrite_compacts_only_log_suffix_before_slab_tombstones`
  now asserts `EdgeMoved { old_slot_index: 1, new_slot_index: 0 }` (survivor visible at slot 0) followed by
  `EdgeMoved { 2 → 1 }`, and `edge_overflow_compaction_does_not_fold_inline_property_bytes_log` drops the
  variant assertion (which claimed *who* folds) while keeping both effects it was guarding: the edge log is
  folded (`overflow_log_head() == -1`) and the inline-property-bytes log state is unchanged. Only
  `vertex_edge_span_rewrite_weights_slack_by_label_degree` remains, and its fix is a *fixture* change, not an
  assertion change: the weighting property (`hot_capacity > cold_capacity`) still holds, but `hot_capacity >
  hot_bucket.stored_slots()` now requires the window to host the slack (slack is a hint), so the fixture must
  build a roomy graph. Copy the construction from
  `gap_tree_full_path_growth_past_5728_releases_only_owned_regions` (it exists precisely because "`test_graph`
  (256 slots) cannot host this shape"): the same `LabeledLaraGraph::new(...)` with
  `crate::labeled::InitialCapacities::uniform(1 << 20)` that appears at compact.rs:5794 — note `LabeledLaraGraph::new`
  takes **18** arguments (my 10-argument attempt did not compile). After that the delegation should be green;
  then land it (−152 lines) with the ADR 0096 §5 note that the publish owns the fold, run canbench
  (`compact`, `ins`), and continue with the final gates. HEAD is 614/0 and everything else from this session is
  committed.
  enabling it).
     decision removes).
  fourth implementation's placement.
  had to be reverted).
  delegation, and it needs these 14 resolved first.
  `/tmp/delegated_compact.rs`.
  (`old_alloc=2 new_alloc=18 old_base=2608 new_base=2608 moved=true leaf=(256, 3664)`, all in-block),
  and the error is raised **before `commit_vertex_edge_span_layout` reaches its positions step** —
  i.e. inside `rewrite_vertex_edge_span`'s non-disjoint inline branch (the one that builds
  `per_bucket`/`raw`). Next instrumentation target is that branch (its `slab_only_bulk` predicate
  evaluation, `per_bucket` collection, and `raw` sizing), not the planning or the commit. Working copies: `/tmp/c_{compact,promote,bucket,leaf_pin,graph}.rs` (C + normalization) and
  the same files with C alone is `/tmp/ssot_*`.
  For reference, the only *self-consistent and complete* variant today is the root-at-prefix-base
  reuse (B): it passes every geometry test, but holds up to `T_PROMOTE` slots (4 KiB) per promoted
  bucket as claimed slack until the leaf is relocated, which is exactly the space cost this
  requirement rejects.
  Working copy saved at `/tmp/promote_inspan_attempt.rs`; regression test at
  `/tmp/tree_promo_test.rs` (`tree_promotion_leaves_the_vertex_cover_tiled_with_its_leaf`, fails on
  HEAD with the exact guard message).
- **Prescribed fix (ADR 0088 §3, no design decision left):** the tree promotion must reserve the
  combined root region through the vertex/leaf tiling (the same `rewrite_vertex_edge_span` /
  leaf-placement path slab growth uses) instead of a bare `edges.allocate_span`, and must then
  publish the vertex cover from the resident length (`bucket_physical_resident_slots`) so the
  vertex's `(base, cover)` describes exactly the region the tree bucket owns. Watch items while
  implementing: the promotion's rollback path currently releases the root region with
  `release_span(new_edge_start, combined_root_len)` (must follow the tiling's owner), the
  in-pinned-leaf release caveat documented at Phase 3c, and `force_tree_mode_for_test`'s
  `force_bucket_to_stored_slots` fixture which prepares the slab prefix.
  Then add the skewed-leaf regression (256 vertices, degrees 1..2048, segment 16) — the reverted
  attempt already has one at `/tmp/tree_promo_test.rs` — re-run the skewed-leaf audit, and resume
  the production balance evaluation. Budget the three interactions listed above as part of the
  slice: they are the reason this is not a drive-by patch. Until then, treat the
  K=4 boundary flip (`91575bc29`) as needing this follow-up before release.

### GAP-2026-09-20-004 — PMA counts tree maintained eagerly with no runtime reader

- **Status:** Fixed 2026-09-20 (commit `f9b276b6e`) — recorded the same day while
  looking for the next lever after the drain/property fixes. Not a threshold issue:
  it affects every slab insert/remove in every arm.
- **Observed behavior (confirmed):** `bump_counts_leaf_with_layout` propagated each
  `actual`/`total` delta up the segment tree with one read + write per level
  (~2.3 K instructions per insert on a 15-level tree). The S1 probe had already
  measured this walk at **56 % of the attributed labeled append cost**
  ([investigation §S1](investigations/2026-09-17-lara-improvement-investigation.md)),
  but every runtime consumer reads the **leaf** row only: the density decision
  (`leaf_segment_counts_for_vid`), the layout audit (`assert_labeled_edge_store_pma_counts`),
  and the leaf cascade. The internal rows had no reader outside the walk itself and
  the segment-growth rebuild — verified by enumerating every `counts.get`/`counts_store().get`
  site (all leaf-indexed except the walk's own read-modify-write, the growth
  migration, and two test seed helpers that recompute the tree inline).
- **Fix:** leaf rows are canonical. The hot path applies its delta to the leaf row
  only; `EdgeStore::rebuild_counts_internal_nodes` (exposed on the labeled graph,
  body shared with the segment-growth migration) recomputes the internal rows from
  the leaves where an aggregate is needed.
- **Tests:** `counts_internal_nodes_are_derived_and_repaired_on_demand` — leaves stay
  exact, the internal root is *stale* before a rebuild (the wrong-impl probe: eager
  maintenance would already match), the rebuild restores every internal row from its
  children, and a further mutation leaves the root stale again.
- **Measured (T_promote = 1024, artifact pre/post of the same persist run):**
  `bench_r_ed_st_si_1024` (1,024 scattered slab appends) 4.58 M → 2.17 M (−52.6 %);
  `bench_l_s2_det_hub_1024` 25.01 M → 17.88 M (−28.5 %, now below both the pre-F1
  20.72 M and the pre-re-tune slab reading); `bench_l_nt_bp_ins_4096` 415.6 K →
  327.9 K (−21.1 %); `bench_l_s2_det_hub_st_4096` −16.8 %;
  `tiny_workload_skewed_mix` (G5) 122.11 M → 115.49 M (−5.4 %). The persist run
  reported 13 improved / 0 regressed / 178 unchanged. Single-leaf growth shapes
  (M1) are unaffected — their walk hits one shallow path, so the total does not
  move. Note: G5's 132.24 M reading in the earlier A/B session predates the drain
  batch and the property cursor, and cross-build canbench comparisons carry a few
  percent of code-layout variance, so only same-run numbers are quoted as deltas.
- **Contract:** [lara-dgap-contract](storage/lara-dgap-contract.md) and
  [lara](storage/lara.md) now state leaf-canonical counts with on-demand internal
  repair (DGAP maintains the tree eagerly; the density semantics are unchanged).

### GAP-2026-09-20-002 — Emptied-bucket span release costs ~26K per call (drain paths)

- **Status:** Fixed 2026-09-20 (partials `914734443`, A2 `a021a309c`) — measured
  performance defect, recorded 2026-09-20. Found while attributing the
  `canbench --persist` verdict for the `T_promote = 1024` re-tune; it is **not**
  threshold-dependent (identical bench totals at 1024 and 4096) and predates the
  re-tune.
- **Observed behavior (confirmed):** F1 (`3fd14768b`) releases an emptied slab
  bucket's span immediately (`release_bucket_edge_span_on_empty`). A detach-delete
  drain empties every neighbour's 1-edge bucket, so `bench_l_s2_det_hub_1024`
  performs 1024 such releases. The pre-F1 artifact value for the same bench is
  20.72 M (no per-bucket release); the value at measurement time was 51.31 M.
  Uninstrumented attribution (native pattern bench
  `fs_drain_release_pattern_1024` + whole-`release()` ablations; the first,
  heavily-scoped probe run over-reported the lookups and is superseded): a
  drain-shaped release costs ~34 K, of which the dup/prev/next lookups are only
  ~0.6 K and the rest is the free-span store's stable-memory writes (~1.3 K per
  write, ~20 writes on the double-merge path), plus ~4 K for the cover
  re-read/sync in the labeled path.
- **Evidence:** [regime-cost investigation](investigations/2026-09-20-tree-regime-cost-improvements.md)
  §Finding A (probe table: `tmp_release_empty` 30.88 M / 1025 calls,
  `tmp_fs_release` 26.73 M, `tmp_cover_recompute` 4.41 M, `tmp_counts_dec`
  9.28 M over 2048 calls); bench A/B at both thresholds (57.09 M each) rules out
  the re-tune; `git merge-base` shows the artifact baseline predates F1.
- **Impact:** every drain that empties many buckets (detach delete, vertex
  delete, churn) pays ~26 K per emptied bucket inside the free-span store; the
  cost is per release, so a 4096-edge hub pays ~4× the 1024-edge one. It also
  explains why the re-tune looked responsible for the drain regression.
- **Owner:** labeled delete path (`labeled/graph/remove.rs`,
  `release_bucket_edge_span_on_empty`) plus the free-span store
  (`lara/edge/free_span.rs`, `release`/`insert_span`). The reclaim owner in the
  replacement design must keep the F1 guarantee (no phantom occupancy after a
  delete) and the GAP-2026-09-17-001 double-free protection.
- **Landed partials (2026-09-20, commit `914734443`):** `release()` needs two
  page-walking lookups instead of four (`predecessor_or_equal` added to
  `ic-stable-paged-ordered-map`; `successor` now covers adjacency *and*
  next-overlap, which also closes a latent hole where a span strictly inside the
  released range was ignored when another span started exactly at the range end
  — regression `release_prefers_inner_overlap_over_adjacent_merge`), and
  `write_record` writes the 48-byte record in one write instead of six.
  Measured: native pattern bench 35.34 M → 33.41 M; `bench_l_s2_det_hub_1024`
  51.31 M → 50.03 M (scoped removal 36.57 M → 35.38 M).
- **A2 landed (2026-09-20, commit `a021a309c`):** the synchronous detach delete opens
  one span-release batch per orientation (`begin_span_release_batch`), emptied
  buckets record their ranges (the per-delete cover sync still runs), and
  `flush_span_release_batch` releases them as merged runs before the delete
  returns — so F1's reuse contract holds at the operation boundary and the store
  sees one insert per contiguous group instead of one per emptied bucket. Tiny and
  tree buckets stay excluded (no span / LTB-addressed). Regression:
  `detach_delete_flushes_emptied_spans_as_merged_runs` (asserts flush runs equal
  the pre-computed merged-run count and that every freed range is allocatable
  afterwards; wrong-impl probe: disabling the batch yields 0 vs 8 runs). Measured:
  `bench_l_s2_det_hub_1024` 51.31 M → 25.01 M (−51 %), `..._4096` 208.00 M →
  102.59 M (−51 %), stepped variant unchanged (−2 %, one release per step by
  design), `..._sat_4096` unchanged.
- **Residual evaluated, not pursued (A3, 2026-09-20):** with the batch in place
  the drain bench sits 4.3 M above the pre-F1 20.72 M, and that difference
  matches F1's *necessary* per-delete cover sync (~4.2 K × 1024 emptied buckets) —
  the per-flushed-run store write cost is only ~1 M of it. Store-level write
  batching therefore has a ceiling of roughly 4 % on this shape; kept as
  `fs_drain_release_pattern_1024` (de-benched) if a release-heavy workload ever
  needs it. The drain is now threshold-independent: both `bench_l_s2_det_hub_*`
  benches measure identically at `T_promote` 1024 and 4096.

### GAP-2026-09-20-003 — Tree property reads resolve the property leaf per row

- **Status:** Fixed 2026-09-20 (commit `cfd389cad`) — measured performance
  defect introduced as a *cost* by the `T_promote = 1024` re-tune
  (property-bearing buckets above 1024 rows are tree now). Fix: the property
  walk caches the leaf's LTB block (`PropertyLeafCache`), resolving and reading
  it once per leaf and serving rows from the buffer in both walk orders
  (`tree_read.rs`); `tcsr_4096_property_read_w32` 4.83 M → 3.66 M at
  `T_promote = 1024` (slab reference 3.22 M, so the residual tree overhead is
  +13.7 % instead of +50 %). Pinned deterministically by
  `tree_property_scan_reads_each_payload_block_once`, which counts LTB payload
  read calls over a promoted 4096-row w = 32 bucket and requires exactly
  `4 + 32 = 36` (edge blocks + property leaves) in both orders — the pre-fix
  shape fails it with 4100.
- **Observed behavior (confirmed):** `visit_edges_with_inline_property`'s tree
  path reads each edge block once but calls `read_property_value_at_slot` per
  row, which re-resolves the property leaf (`resolve_property_leaf_block_id`:
  a LEG root read plus offset math) and issues a separate 4-byte LTB read per
  row. At w = 32 (K = 128 rows per property leaf) a 4096-row scan pays 4096 root
  reads + 4096 partial reads instead of 32 block reads:
  `tcsr_4096_property_read_w32` 3.22 M (slab) → 4.83 M (tree), +50 %.
- **Evidence:** [regime-cost investigation](investigations/2026-09-20-tree-regime-cost-improvements.md)
  §Finding B; same-bench A/B at `T_promote` 4096 vs 1024 with all other code held
  constant.
- **Impact:** property-bearing scans over buckets above `T_promote` rows pay the
  per-row resolution; w = 0 scans are unaffected (M2a improves in tree mode).
- **Owner:** `labeled/graph/tree_read.rs` property-bearing scan loops
  (ascending/descending) + `read_property_value_at_slot`.
- **Next decision:** none for the per-row resolution itself. The residual
  +0.44 M over the slab reference is the tree property indirection (one leaf
  resolution plus one 4 KiB block read per leaf, per-row closure value clone);
  revisit only if a property-scan-heavy workload makes it material. The
  deterministic test also gives the LTB store a reusable payload-read counter
  (`ltb_payload_read_calls`) for future block-granularity contracts.

### GAP-2026-09-20-001 — Promote silently drops overflow-log rows (tree mode has no log)

- **Status:** Fixed 2026-09-20 (commit `164e27cfa`), then **closed by deletion 2026-09-20 (ADR 0097)**:
  the shared log is gone, and `promote_bucket_to_tree_mode`'s Phase 0a compacts the bucket first - the
  run's live rows into the prefix (ADR 0097 §7) and the value log into the value slab - so the
  transcription reads a prefix that already covers every resident row and the failure mode is
  unrepresentable. The retired fold's span-growth retry survives as the compaction's retry.
  Originally recorded while
  triaging the `T_PROMOTE = 1024` flip (found by classifying the 11 failures that
  appear at the lower threshold; see GAP-2026-09-17-001 for the density-accounting
  context). Fix: promotion folds the slab overflow log into the prefix first
  (`ensure_label_bucket_folded_to_slab`, with the vertex-span rewrite retry), then
  re-locates the descriptor and transcribes the folded prefix; a fold that cannot
  make room fails closed and leaves the slab bucket and its log intact. This is
  the route ADR 0088 §7's transcription clause allows and §6 ("promotion always
  folds") names; the rejected alternative was interleaving log entries directly
  during transcription (larger, and duplicates the fold's ordering rules).
  `assert_labeled_layout_invariants` now enforces the tree wire rules
  (`overflow_log_head < 0`, `degree <= stored_slots`) and sizes a tree bucket's
  on-slab range from `bucket_physical_resident_slots` (the logical width had
  false-positived against the LEG capacity). Cost: `tcsr_4096_insert_grow` 69.92M
  instructions, no change vs the persisted artifact (the fold runs only when a log
  is active).
- **Observed behavior (confirmed):** `promote_bypass_to_tree_mode` transcribes
  exactly `pre_stored_slots` prefix slots into the LTB blocks and publishes
  `overflow_log_head = -1` ("the log was orphaned by promote"). Any live row the
  slab **overflow log** holds at the trigger is therefore dropped — tree mode has
  no log (ADR 0088 §2) — and the row becomes unreachable. Post-promotion the
  bucket reports `degree = stored_slots + N` (N = log depth at the trigger) while
  every scan (forward and reverse) returns `stored_slots` rows. The inserts that
  admitted those rows returned `Ok` and `num_edges` counts them: fail-open.
- **Evidence / repro:** temporary probe (removed after diagnosis) seeding a
  width-2 `InlinePropertyTestEdge` hub through the production
  `insert_directed_edge` path on `valued_bidirectional_graph()`:
  - `T_PROMOTE = 1024`: promotion fires at insert 1191 →
    `stored 1191, degree 1192, out/in scans 1191`.
  - `T_PROMOTE = 4096`: promotion fires at insert 4251 →
    `stored 4251, degree 4252, out/in scans 4251` → **threshold-independent**.
  - width-0 control: exact (`stored == degree == scanned` at 4097/5000/8000);
    the trigger is a *non-empty overflow log*, which the width>0 path reliably
    produces because the edge prefix plateaus (`stored 4080` while `degree 4200`,
    `log_head 119`) while the property stream keeps growing.
  - Regression: `gap_promote_with_active_overflow_log_keeps_every_row` (fails
    before the fix with `left: 4999, right: 5000`; with the audit swapped in it
    fails with `vertex 2 bucket 256: tree degree 5000 exceeds stored width 4999`).
    The lower-threshold test `directed_inline_property_adjacent_reverse_hub_stays_writable_after_skew`
    (`in_edges_for_label(..).len() == 2000` → 1999) exposed it originally.
  - No detector fires today: `assert_labeled_edge_store_pma_counts` compares
    `actual` against bucket degrees (both agree on the inflated degree), and the
    tree block-header parity helper is test-local.
- **Impact:** silent adjacency loss for any promoted bucket that carried
  log-resident rows. Promotion frequency rises as the threshold falls, so the
  `T_PROMOTE = 1024` flip multiplies the exposure; the flip is gated on this fix.
- **Owner:** tree promotion transcription (`labeled/graph/promote.rs` Phase 2/3)
  plus overflow-log ownership in the labeled insert path
  (`labeled/graph/insert.rs`; the slab path's fold helper
  `fold_label_bucket_*_log_to_slab` already exists). Contracts: ADR 0088 §2
  (tree mode has no overflow log), ADR 0096 §5 (log rows are PMA edge records).
- **Next decision:** smallest fix — fold the overflow log into the prefix before
  promoting (fold → re-check trigger → promote, reusing the existing fold path),
  versus a typed fail-closed precondition on `overflow_log_head() >= 0` that makes
  the caller fold-then-retry, versus transcribing prefix+log rows into the tree
  (larger). Regression: promote a bucket with a non-empty log, then assert
  `degree == stored == scanned` for both directions plus payload identity.

### GAP-2026-09-17-001 — Tree-mode full-path 4-byte bucket growth traps past ~5.7K edges on an overlapping edge free-span release

- **Status:** Fixed 2026-09-19 (commits `c664d8f73`, `c779ced27`, `0e2ac456a`,
  `a86ff0f50`; density-accounting half `8a6d73925` + audit coverage `795754dca`) —
  root cause was a unit confusion shared by SIX paths, not one call site.
  Every resident-geometry computation sized tree buckets on the logical
  `stored_slots` edge count while the physical LEG span is only the root region
  (`combined_span_region_len`):
  (1) `release_vertex_edge_span_footprint`'s monolithic whole-cover
  `release_span(span_start, span_len)` handed unowned ranges to the free store
  (the observed `OverlapPrevious` trap); (2) `plan_labeled_leaf_relocation`
  over-allocated the leaf block (5728 slots for a 6-slot root), which is what made
  the whole-cover release overlap live free spans; (3) slide tiling, (4) rebalance
  planning, and (5) fold/grow-footprint planning shared the same logical-width
  sizing; (6) `materialize_labeled_vertex_edge_plan` shared it on the READ side,
  snapshotting tree buckets as `stored_slots` slab slots from `edge_start`,
  walking past the root region into live ranges and republishing those bytes as
  the bucket's new span on relocate (corruption: post-relocate scan returned 1
  edge instead of 8192 — no trap, so fail-closed never fired). The fix introduces
  `bucket_physical_resident_slots` (compact.rs) as the single source of truth —
  tiny 0, slab `stored_slots`, tree `combined_span_region_len` (log-chain slots
  still added by callers) — routes all six paths through it (plus the commit
  rebuild, which preserves tree descriptors' logical width: root bytes move by
  anchor only), removes the whole-cover release so footprints retire as mode-aware
  bucket regions plus remainder only, and unifies the retire-interval filter/width
  and live-guard on the same helper (commit `0e2ac456a`; value-identical, no
  behavior change). Standing regression:
  `gap_tree_full_path_growth_past_5728_releases_only_owned_regions` (compact.rs
  tests; M1 shape 0→8192 full-path inserts, asserts tree mode, root-region width,
  + full adjacency; bisect checkpoints at every 512 inserts all 1:1).
  Threshold A/B completed 2026-09-19 on the fixed path (unpersisted runs, both
  arms; T_PROMOTE reverted to 4096 after measuring — production unchanged):
  M1 hub-grow 82.30M (4096) vs 92.89M (1024, ~13% worse); M2a scan 74.30K slab vs
  40.09K tree (~1.9x tree-favored); M2b insert 8,364 slab vs 185.07K tree (~22x
  tree append cost); M2c delete 4,845 vs 7,887; M4 churn 147.54M vs 38.64M
  (threshold-relative sizing artifact — the 1024 round-trip does less work by
  definition, not evidence of threshold merit). Verdict at the time: the M2a
  tree-scan win is outweighed at workload scale (M1); **T_PROMOTE stays 4096**
  (evidence-backed, not freeze-by-default). **Superseded** by the
  density-accounting fix below: post-fix M1 inverts (1024 25% cheaper on equal
  work).
- **G4/G5 follow-up (2026-09-19, commit `a5cefbf69`):** G4 (`tiny_relocate_mixed_leaf`)
  and G5 (`tiny_workload_skewed_mix`) benches landed; both green unpersisted (G4 ~594K
  ins, HI=0/SMI=0 with payload-identity + per-neighbor scan asserts; G5 ~140M ins for
  256v/4520e with exact census assert). Two discoveries: (1) a tree bucket co-resident
  with pinned mates on one vertex is unseedable in quota-1 leaf geometry BOTH ways
  (tree-first stalls mate tiles at the 8th edge via the leaf-mate overlap assertion;
  mates-first stalls the tree growth cascade at ~4703-4895 edges) — tree-under-relocate
  stays covered by the M1 regression instead; (2) the G4 probe exposed a stale ADR
  expectation: tiny anchors ADVANCE on relocate (running-boundary stamps per the
  Successor-chain row), so G4 asserts payload identity, not anchor identity.
- **T=1024 follow-up (2026-09-19, commit `a86ff0f50`):** the threshold A/B's T=1024
  arm trapped M1 at edge 2063 with `DuplicateStart` on `release_span(1048579, 2064)`
  — a SECOND double-free of the same family, at a different site: the tree
  combined-span realloc (w==0, no avoid) re-took a freed head `[1048579,2)` for the
  live root, and the next leaf relocate released the stale whole cover over it
  (plus the symmetric tree step-7 old-span release). Both whole-cover releases now
  delegate to `release_vertex_edge_span_slab` (skip-free-prefix + probe): live
  covers release whole (identical behavior), recycled heads release only the owned
  remainder. Verified: T=1024 diagnostic 0→8192 full-path growth green with full
  adjacency. Rejected alternatives: `allocate_span_avoiding` re-release removal
  (leaks the taken span as unaccounted padding — the range stays reserved but
  untracked); free-store-overlap guard at the leaf-release choke point (wrong layer:
  legitimately-abandoned covers routinely overlap recycled ranges after slide
  republishes spans — the guard would convert every such release into a leak);
  old/new-block overlap check at the relocate call site (necessary but insufficient:
  the overlap is with the NEW block's recycled free tail, not the old cover).
- **Second-stage hardening (evaluated 2026-09-19, NOT pursued):** a `SpanExtent`
  enum (slab/tree unit separation at the type level) was scored highest (8/10) but
  rejected on implementation review — the three release call sites correctly pass a
  *logical cover* (mixed-mode vertex width), so forcing physical widths at the call
  boundary contradicts the valid calling convention; the remaining divergence was
  mechanical and is now unified (see above). NOTE (precision, 2026-09-19): the
  SpanExtent score applies to the ORIGINAL gap (logical/physical unit confusion)
  only — it does not address the T=1024 follow-up (temporal ownership overlap),
  which no type can capture. A dedicated `TreeSpanLengthMismatch`
  error variant was prototyped and reverted: the code no longer constructs the
  failure condition, so the variant would be unreachable (YAGNI); the free-store
  `OverlapPrevious` tripwire already covers this class (proven: it caught the
  original trap). The dual meaning of `vertex.stored_slots` (logical cover vs
  physical width) remains and is *not* what plan 0361's R4 addressed: R4
  privatized the **bucket** descriptor's `stored_slots` (mode-aware accessor,
  landed 2026-09-20 `15719602c`); the vertex-level cover/resident split is owned by
  the resident-geometry SSOT introduced here (`bucket_physical_resident_slots`)
  and stays a documented convention rather than a type split.
- **Future direction (proportional trigger: a THIRD same-family double-free):**
  an arena rule — while a leaf block is pinned, defer per-bucket sub-releases
  inside it and release only whole abandoned blocks — would make recurrence
  structurally impossible (E1=5) instead of guarded per site (current: E1=4).
  Cost: pin-liveness checks on every sub-release path, new state + invariant
  design (separate slice). The current slab-fallback delegation bounds the
  residual hole (stale covers skip free ranges; live spans are republished
  before old-cover release per post-slide invariant, pinned by M1/G4/G5/suite).
  Do NOT pursue before a third firing — disproportionate until then.
- **Density-accounting follow-up (2026-09-20, this slice):** the second half of the
  same trap. Tree inserts bumped leaf `actual` (+1 per insert, −1 per remove) even
  though tree rows live in LTB blocks and occupy only the root region, and the
  promote did not subtract the slab-era degree; the leaf audit
  (`expected_vertex_pma_contribution`) counted tree `degree()` too, so the
  accounting stayed self-consistent while driving density ≥ 1.0 after every
  promotion — each post-promotion insert fired
  `rebalance_cascade_after_labeled_mutation` (M2b: 185.07K vs 8.36K; M1: 92.89M
  vs 82.30M at T=1024). Fix: `actual` now means *live edge records that occupy
  edge-slab slots*. Tree insert/remove helpers no longer take a vertex id
  (structurally unable to touch per-vertex counts); `promote_bypass_to_tree_mode`
  subtracts the live degree (`−degree`, LARA parity `segment_actual[leaf] -=
  live`); `tree_mode_demote_to_slab` re-adds it (`+degree`, mirroring
  `promote_tiny_to_slab`); the leaf audit and the batch `RunDestination::Tree`
  commit skip the bump. Tests:
  `tree_mode_leaf_actual_counts_slab_edges_only` (production insert/remove/
  promote/demote paths, exact leaf counts + audit at every step) and the tree-run
  assertion in `batch_run_admits_tree_mode_bucket_tail_fit`;
  `force_bucket_to_stored_slots` now seeds the matching leaf count so fixture
  states audit cleanly. The M1 growth regression
  (`gap_tree_full_path_growth_past_5728_releases_only_owned_regions`) now
  also asserts leaf `actual == 0` after 8192 full-path inserts and runs the
  leaf audit, so the rule is pinned across every relocate/slide/rebuild step
  of the growth chain, not only at the transition sites. Wrong-impl probes (re-added insert bump, removed promote
  subtract, removed demote re-add, restored batch bump) each fail those tests.
  ADR 0096 §5 updated; ADR 0088 §3 clarified (root *span* residency stays
  mode-blind; tree *edge rows* never count).
- **Threshold verdict — SUPERSEDED by the density-accounting fix (same session,
  2026-09-20).** Re-measured both arms with the same unpersisted `thresh_*`
  benches after the fix (`T_PROMOTE` reverted to 4096 after measuring; production
  constant unchanged):

  | Metric | T=4096 | T=1024 | Pre-fix |
  | --- | --- | --- | --- |
  | M1 hub-grow 8192 (equal work) | 55.87M | **41.70M** | 82.30M / 92.89M |
  | M2a scan 2048 | 74.30K | **40.09K** | 74.30K / 40.09K |
  | M2b insert into 2048 (block-boundary mint) | **8.36K** | 63.51K | 8,364 / 185.07K |
  | M2b insert into 2050 (steady-state append) | 8.36K | **4.46K** | not measured |
  | M2c delete 2048 | **4.84K** | 7.33K | 4,845 / 7,887 |
  | M4 churn round-trip (threshold-relative sizing) | 117.65M | 30.77M | 147.54M / 38.64M |
  | G5 skewed mix 256v/4520e (workload level) | 140.11M | **132.24M** | not measured |

  Both arms improved on M1 (the false cascade is gone) and the **ordering
  inverts**: 1024 is now 25% cheaper for equal work (8192 full-path inserts), so
  the pre-fix rationale for keeping 4096 no longer holds. M2b's residual 7.6× is
  a block-boundary mint (2048 → 2049 mints an LTB block, grows the root, reallocs
  the combined span), not a cascade: with the seed moved off the block boundary
  (2050) the same append costs 4.46K in the tree arm vs 8.36K in the slab arm, so
  tree appends are ~1.9x cheaper in steady state and the earlier 7.6x was the
  one-off mint. The insert/scan-dominated G5 workload mix also favors 1024
  (132.24M vs 140.11M, -5.6%). Every equal-work and workload-level metric now
  favors 1024; only single deletes (7.33K vs 4.84K) and boundary mints favor
  4096, and hub-delete streams are not a workload the Orkut comparison
  exercises. **Gated:** the flip is also blocked by
  [GAP-2026-09-20-001](#gap-2026-09-20-001--promote-silently-drops-overflow-log-rows-tree-mode-has-no-log)
  (promotion silently drops overflow-log rows, and a lower threshold multiplies
  the number of promotions). The drain batches landed later the same day
  (`a021a309c`) make the delete side threshold-independent, so the cost record
  is now property-scan indirection (+13.7 %) and the bounded boundary mint only.
  **Decision taken (2026-09-20):** the flip was executed
  once the gate cleared — `T_PROMOTE = 1024` / `T_DEMOTE = 512` (commit `2b417059f`),
  with the ten threshold-coupled tests migrated to derive their sizes from
  `T_PROMOTE` so the suite is green at both constants (602/0 each way). The density
  fix is threshold-agnostic; the flip only changes where promotion happens.
  Persisted artifact re-measured the same day (unfiltered `canbench --persist`,
  191 benches, 11 regressed / 5 improved / 3 new). Attribution was corrected by
  measurement (2026-09-20, temporary scopes; see the
  [regime-cost investigation](investigations/2026-09-20-tree-regime-cost-improvements.md)):
  only the property-bearing scan `tcsr_4096_property_read_w32` 3.22M → 4.83M and
  the once-per-B-rows block-boundary mint are re-tune costs. The hub-drain
  regression (`bench_l_s2_det_hub_1024` 20.72M → 51.31M, `..._4096`
  85.49M → 208.00M) is **threshold-independent** and comes from F1's
  per-emptied-bucket span release (`3fd14768b`) — see GAP-2026-09-20-002 — so the
  persist commit `da852423d` message misattributes it. `bench_remove_churn_*`
  scope growth is attribution only (totals +2%). Revisit trigger: a
  property-scan-heavy target workload re-opens the threshold (2,048 keeps
  4,096-row property buckets on the slab side).
- **Observed behavior (confirmed):** full-path `insert_edge` (impl + dense-check +
  cascade) on a single-vertex/single-label `LabeledLaraGraph` with 4-byte edges
  (Insertion policy), growing 0 → 8192, traps deterministically at the 5728th edge:
  `Store(RebalanceFailed(GrowFailed { current_size: 0, delta: 0 }))` with the bucket
  tree-mode, `stored = degree = 5728`, `log_head = -1`. Isolation worktree at detached
  HEAD `95d791087` (no in-flight changes) fails identically — latent HEAD bug, not an
  in-flight regression. Throwaway tracing in the isolation worktree shows an EDGE-slab
  `release_span start=1063168 len=5728` rejected with `OverlapPrevious { previous:
  FreeSpan { start_slot: 1059545, len: 3628 }, inserted: FreeSpan { start_slot: 1063168,
  len: 5728 } }` — overlapping exactly the 5 slots of an earlier `len=5` release at the
  same start. A `stored_slots`-scale (5728) release is issued from the tree growth
  regime where a root-region-scale (or no) release belongs.
- **Observability note:** the surfacing error misdirects. `impl From<GrowFailed> for
  LabeledOperationError` (`crates/ic-stable-lara/src/labeled/graph/error.rs:336`)
  wraps ANY `GrowFailed` as `Store(RebalanceFailed(..))`; instrumenting the only
  direct `RebalanceFailed` constructor (`labeled/bucket_store.rs:478`) never fired.
  The fault is in the edge-slab free-span path (`lara/edge/span.rs`), reached via the
  generic `From` conversion. Consider distinct error mapping as a follow-up; not part
  of this gap's fix.
- **Expected or needed behavior:** tree growth to the structural cap (2^30, ADR 0088)
  via the production path; release lengths in root-region units, never
  `stored_slots`-scale overlapping ranges.
- **Owner:** labeled tree growth + PMA accounting: `tree_write.rs` release sites,
  `compact.rs` leaf-relocate footprint sizing, per-insert counts bumps in
  `labeled/graph/insert.rs:495,814` (the same ownership the Tree-CSR workstream
  had; that workstream landed in `main` before this entry was closed).
- **Evidence:** [2026-09-17 investigation, §A result](../investigations/2026-09-17-lara-improvement-investigation.md);
  repro shape `thresh_hub_grow_8192` (M1 bench, worktree `labeled/bench.rs`);
  native repro `head_repro_full_path_growth` in the kept isolation worktree
  `/tmp/gleaph-head` (detached HEAD + throwaway eprintln tracing in
  `bucket_store.rs`/`span.rs`): `cargo test -p ic-stable-lara --features canbench
  --lib head_repro`. Unit-level promote paths are healthy (23/23 promote tests pass) —
  the failure is steady-growth + cascade interplay, untested until now (4B benches
  seed via `skip_leaf_cascade`; full-path growth benches are 10B-only and never promote).
- **Impact:** tree mode is unusable past ~5.7K edges via the production insert path;
  threshold A/B (1024 vs 4096) cannot run; S5 adoption is blocked.
- **Next decision:** answered (2026-09-20) — tree-mode inserts did bump leaf
  `actual`, and `release_labeled_leaf_physical_footprint` was sized by a
  slab-unit footprint; both halves are fixed (see the follow-up bullets above).
  Remaining decision tracked there: whether to adopt `T_PROMOTE = 1024` now that
  the post-fix evidence favors it at workload scale.

### GAP-2026-09-12-001 — Exact-vertex bulk updates do not support EXISTS-chain policies

- **Status:** Open — deferred capability, recorded 2026-09-12.
- **Observed behavior:** Fixed-vertex SET/REMOVE admits a labeled NodeScan and property residuals.
  EXISTS policy lowering needs SemiApply/reverse-seed joins, outside that Graph input contract.
  Router deliberately rejects applicable target-label chain policies before scalar reservation
  or Graph dispatch with `NotImplemented("bulk vertex updates with EXISTS policies are not supported")`.
  Property-only conditional grants, including indexed equality, are supported. The policy is never
  ignored, and the rejected row leaves no scalar dispatch for Abort to settle.
- **Owner:** Router `policy_pushdown.rs` / `gql::lower_for_execution`; Graph
  `plan_wire_guard::validate_mutation_target_plan` owns exact-input admission.
- **Evidence:** `pure_exists_row_lowers_to_one_bounded_semi_apply_probe` checks ordinary chain
  lowering and the bound-input rejection; `bound_vertex_keeps_indexed_policy_as_a_full_residual`
  checks the supported property form. These are native tests, not an EXISTS bulk E2E claim.
- **Impact / needed behavior:** A non-tenant whose target-label visibility needs a chain cannot
  perform bulk vertex updates under that policy. Ordinary GQL policy execution is unchanged.
- **Next decision:** Specify bounded chain evaluation over the one saved vertex and its replicated
  execution path before extending the exact-input shape. Do not substitute an index-selected target
  or drop residual authorization checks.
- **Contract:** [ADR 0057](adr/0057-router-operation-api-and-durable-bulk-load.md), update-lane
  policy and exact-target constraints; [plan format](gql/plan-format.md), exact vertex mutation input.

### GAP-2026-09-11-004 — Bulk-load edge property update needs replicated resolution and retry-safe target validity

- **Status:** Open — prerequisite slice. Recorded 2026-09-11 after the edge-property SET review
  round withdrew the implemented surface (wire variant, CLI mode, Router path, docs section) from
  its slice rather than shipping a weakening contract. Edge-property SET through bulk load is
  **not implemented**. The vertex update lane's journal-first resume/abort work in the same round
  did not re-add any edge surface, identity, or lock substrate.
- **Owner:** Router bulk-load workflow (`crates/router/src/bulk_load.rs`) owns admission and
  replay. Graph owns canonical adjacency, edge-target validity, and any Router→Graph resolution
  API (`crates/graph/src/lib.rs`, GraphStore, and the underlying LARA mutation boundaries).
- **Observed behavior (confirmed):** Router ingress handlers execute in replicated mode. The
  withdrawn Router→Graph pre-read of *edge state* used the composite query
  `execute_plan_query` (`#[query(composite = true, guard = "guard_router_canister")]`), which a
  replicated-mode handler cannot call. A PocketIC run of the withdrawn edge-update lifecycle
  failed with `InvalidArgument("graph execute_plan_query call failed: call rejected: 5 - IC0527:
  Composite query cannot be called in replicated mode")`, so an `Append` handler could not
  evaluate the required exactly-one count before admission.
- **Observed behavior (what already exists, confirmed):** Edge property indexes and Router-side
  edge index lookup are implemented: `indexed_catalog::active_edge_physical_index`,
  `router/src/index_lookup.rs` `collect_edge_equal_hits_paged` (`lookup_edge_equal_page`) and
  `collect_edge_range_hits_paged` (`lookup_edge_range_page`), the Router wire helpers
  `lookup_edge_equal_wires`/`lookup_edge_range_wires` (`router/src/gql.rs`), and the planner's
  `PlanOp::EdgeIndexScan` (`gql-planner/src/plan.rs`). The earlier statement that no edge property
  index exists was wrong.
- **Replicated-call finding (2026-09-12, source/specification evidence):** Edge index paging
  endpoints are ordinary, non-composite queries. The [IC interface specification](https://docs.internetcomputer.org/references/ic-interface-spec/)
  permits update execution to call update and ordinary query methods. The earlier call-mode
  uncertainty is resolved at the protocol level, not by a new bulk-edge runtime test. These APIs
  select property values and return `EdgePostingHit { shard_id, owner_vertex_id, label_id,
  slot_index }`, not endpoint-pair adjacency or a destination/generation witness. Posting count
  alone therefore cannot establish canonical multiplicity for `(from, label, to)`. Graph's
  `execute_plan_update` is not a read-only substitute: its wire guard rejects read-only plans.
- **Target-validity finding (2026-09-12, source/test inspection):** `GlobalEdgeId` is a 16-byte
  query-time physical handle `(shard, owner, label, slot)`, not a lifetime occurrence ID. LARA's
  `unordered_scalar_insert_reuses_interior_slab_tombstone` deletes target 20 at slot 1 and inserts
  target 21 into the same slot. Graph maintenance also moves slots and relocates sidecars/postings.
  Saving the handle does not prevent an unstarted row from updating a replacement edge; checking
  the destination still cannot distinguish delete/reinsert with identical endpoints. The latter
  is an identity-model counterexample, not a newly executed test. Repairing the read API alone
  cannot establish the retry contract.
- **Selected direction (2026-09-12; planned, not implemented):** Preserve multi-edges; select and
  update all eligible matches at each input row's first Graph execution, rather than pinning a
  singleton edge before admission. Zero matches succeeds with target count zero. Exceeding bounded
  work/mutation/byte limits rejects before writes. Selection, canonical SET and durable outcome
  must complete locally without an intervening external call; retries replay recorded outcomes.
  This replaces the withdrawn zero/many-rejection requirement for edge updates, not the existing
  vertex bulk contract. See [selected direction and storage accounting](investigations/2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting).
- **Evidence:** `crates/graph/src/lib.rs:80` (composite-query graph read),
  `crates/router/src/graph_client.rs:100-119` (mode→method mapping),
  `crates/router/src/gql_search.rs:1637` (query-mode dispatch),
  `crates/router/src/index_lookup.rs:185,216` (edge equality/range lookup),
  `crates/router/src/facade/stable/indexed_catalog.rs:267` (`active_edge_physical_index`),
  `crates/gql-planner/src/plan.rs:414` (`PlanOp::EdgeIndexScan`),
  `crates/graph/src/facade/store/handle.rs:10` (internal edge handle), and the failing PocketIC
  run recorded in this review round.
- **Research:** [Bulk edge SET prerequisites](investigations/2026-09-12-bulk-edge-set-prerequisites.md)
  records the API/identity evidence, existing journal/receipt extension points, alternatives, and
  validation gates against `95d791087`. No implementation or new runtime validation was performed.
- **Candidate design (2026-09-12):** [Sparse edge-reference tokens](investigations/2026-09-12-sparse-edge-reference-tokens.md)
  explores a bounded Graph registration map and non-reusing allocator, without widening every
  vertex or edge. Internal LARA materialize/sibling drains use no-op observers, so existing Graph
  callbacks alone do not establish complete invalidation coverage. Bounded resolution and the
  availability cost of capacity-driven invalidation also remain gates. No design approval,
  MemoryId allocation, implementation, or runtime validation is claimed.
- **Alternative research (2026-09-12):** [Primary-source comparison](investigations/2026-09-12-edge-reference-alternatives.md)
  identifies occurrence IDs with bounded source/label lookup as an alternative that requires no
  global ID-to-location directory. Kuzu's inspected update code uses this lookup organization;
  applying it to Gleaph would require wider edge records and identity-preserving rewrites.
  The all-edge ID proposal was declined on storage/performance grounds; no measured throughput
  regression is claimed.
- **Fixed-budget research (2026-09-12):** [Logical-membership witnesses](investigations/2026-09-12-bounded-edge-membership-witnesses.md)
  combines canonical singleton selection with a fixed-size hashed revision array for edge
  insertion/deletion. Adjacency rows stay unchanged; pure movement need not notify the witness,
  and an unusable saved slot rejects rather than starting another scan. Complete membership-write
  coverage, storage-mode/work bounds, metadata-write cost, and bulk-level hash false conflicts
  remain unclosed gates. No implementation or runtime validation was performed.
- **Bounded-read prerequisite (2026-09-12 23:34:39 UTC +0000):** Implemented tree visitor
  short-circuiting in LARA; [ADR 0088](adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#tree-visitor-short-circuit-implemented)
  owns the contract and measurements. Previously, the topology and inline-property adapters saved
  a callback's `Break` but the underlying walkers continued through later blocks/properties.
  `tree_visit_break_stops_topology_reads` and `tree_visit_break_stops_property_reads` reproduced
  both defects and now prove physical stopping in both orders. This is not complete selection:
  `read_label_bucket_placement_info` counts the overflow chain, and selected-slot readers can
  reconstruct chains/prefetch tables outside the emitted-row count. Those existing APIs are not
  a whole-row work/byte bound; no new validity registry is needed.
- **Complete-topology prerequisite (2026-09-13 00:22:26 UTC +0000):** LARA now exposes
  `collect_edge_topology_bounded` and its forward adapter; [ADR 0050](adr/0050-lara-traverse-read-api.md#bounded-complete-topology-collection-implemented)
  owns the contract. A physical-slot allowance includes tombstones and linked overflow entries;
  exhaustion returns an error, never a successful prefix. No property stream or whole-leaf log
  table is read. Workspace and topology payload are bounded from the slot allowance and concrete
  edge width; this is not an independent byte-budget API. Six native contracts and focused
  canbench cover the storage capability, not bulk SET or policy/mutation atomicity.
- **Inline-read prerequisite (2026-09-13 01:26:43 UTC +0000):** LARA exposes a complete topology plus
  `InlinePropertyBytes` collector with physical-slot and value-body allowances; [ADR 0050](adr/0050-lara-traverse-read-api.md#bounded-complete-inline-property-collection-implemented)
  owns the bounds. Value admission precedes property reads, reserves wide-blob lookahead, validates
  exact property-log exhaustion, and preserves slab live ordinals/tree physical slots. Six native
  contracts and focused canbench validate this read capability, not policy/SET atomicity. The tree
  property helper's out-of-range block-zero sentinel was replaced with a typed rejection.
- **Sidecar pre-read blocker (2026-09-13, source inspection):** `facade/stable/edge_properties.rs`
  stores unbounded `StoredPropertyValue`s. The ic-stable-structures 0.7.2 B-tree getter materializes
  the whole encoded value before its `Storable::from_bytes` call; `ensure_persistable` only checks
  binary encoding. A size check after the ordinary getter cannot establish a pre-read byte cap.
  No sidecar format, length map, dependency fork or whole-row mutation preflight was introduced.
- **Sidecar node-discovery evidence (2026-09-13 01:52:13 UTC +0000):** A native 0.7.2 B-tree probe
  with fixed 14-byte keys and unbounded raw values confirms that a ten-byte lookup, a missing-key
  lookup and key-only iteration all perform more metadata reads as an unselected neighbor grows.
  `Node::load_v2` reconstructs the whole node overflow list before key search, so exposing only a
  value length would not close the work bound. The [investigation and reproducer](investigations/2026-09-13-sidecar-bounded-read-options.md)
  compare a dependency-owner extension, a global property cap and out-of-line storage. That
  investigation recommended an extension as the minimum layout-preserving change; the design
  direction below supersedes that priority. This is primitive read evidence, not Graph codec,
  write-budget, policy or bulk SET validation.
- **Property-value separation design (2026-09-13 03:57:44 UTC +0000):** The
  [design](investigations/2026-09-13-property-value-separation-design.md) keeps per-property
  keys and bounded small cells in the existing vertex/edge directories, with independent large
  bodies owned by each PropertyStore. A reclaimable fixed-block allocator is an experiment, not
  an accepted production layout. Threshold, fragmentation, common-path cost, rekey ownership and
  whole-row preflight remain validation gates. No production Rust, MemoryId, persisted codec,
  dependency or public API changed; no extra adjacency IDs or reference journal are proposed.
- **Value-separation experiment (2026-09-13 06:23:23 UTC +0000):** Two native tests over 24
  configurations passed, including body admission before reads, zero-write rejection, ownership
  transfer and persistent block reuse. The [results](investigations/2026-09-13-property-value-separation-design.md#8-probe-results-and-disposition)
  demonstrate removal of unselected-body read/write coupling. They do not select the fixed-block/
  replacement-body candidate: short-body waste, retained high-water and ordinary-path preparation
  cost require further design. A partial-pair reopen defect was found and fixed in the new probe,
  not in production Graph. No Graph codec, Wasm instruction, policy, whole-row SET or IC rollback
  proof is claimed; the existing storage and public capability remain unchanged.
- **Comparison-first follow-up (2026-09-13 09:36:59 UTC +0000):** The
  [B-tree body comparison](investigations/2026-09-13-property-body-btree-comparison.md) measures five
  whole/chunk configurations, with two native tests and 188 I/O rows. Default-geometry 4 KiB chunks
  reduce the small external-body neighbor rewrite from 1048708 to 16582 bytes, but rewrite a 1 MiB
  body with about 5.30× the whole-body write volume. Declared 4 KiB bounds occupy 27 native pages
  for the tested 256 short bodies versus one with default geometry; shrink can also allocate during
  tree mutation. These are diagnostic observations, not Graph instruction or workload acceptance.
  The [extent proposal](investigations/2026-09-13-property-body-extent-design.md) was not measured
  by this B-tree slice; its later native checkpoint is recorded below, without production selection. Body-key/format/move ownership, decoder and
  total-work admission, whole-row co-write and IC lifecycle remain open; no production region,
  allocator, dependency or public capability changed.
- **IC body-lane comparison (2026-09-13 11:08:59 UTC +0000):** The
  [stable-memory follow-up](investigations/2026-09-13-property-body-btree-comparison.md#ic-stable-memory-follow-up)
  completed 32 canbench cases using direct Ic0StableMemory and the shared native body algorithm.
  Default-geometry 4 KiB chunks reduce a small external-body neighbor rewrite from 2.62M to 0.188M
  instructions, but a full 1 MiB replacement costs 28.32M versus 6.94M, and shrink 38.27M versus
  5.06M. Ordinary Whole point-get is also retained as a faster read control than range/assembly.
  All measured heap/stable page deltas are zero after setup; this does not mean zero allocation.
  The two native tests and all 188 I/O rows remain unchanged. Query benchmarking is not Graph
  commit/rollback or workload acceptance; no production layout or custom allocator is selected.
- **Workload relevance (2026-09-13 11:35:15 UTC +0000):** The
  [codec-based input profile](investigations/2026-09-13-property-body-workload-profile.md) covers
  Knowledge and Social 1x1/5x20 typed load artifacts. Maximum Value size is 143 bytes; none exceeds
  4 KiB. Social scaling repeats 71 post texts, not a sampled large-body tail. At hypothetical T=128,
  300/35932 scaled vertex-property occurrences and 0/94460 edge-property occurrences remain external,
  but counts are not access frequencies (all Social prepared queries project Post.body). No runtime
  read/rewrite/shrink history is available from these inputs; large-row test fixtures are synthetic.
  This does not select a cap/threshold or justify custom allocation; representative old/new sizes
  and operation counts remain necessary for workload-weighted acceptance.
- **Chunk-size sensitivity (2026-09-13 12:30:53 UTC +0000):** The
  [wider-chunk comparison](investigations/2026-09-13-property-body-chunk-size-sensitivity.md) extends
  the same body algorithm to 16/64 KiB default-geometry caps: two native tests, 260 I/O rows and
  48 IC query benchmarks pass; the original 188 native rows/32 IC counters are unchanged. Wider
  chunks improve some costs versus 4 KiB, but 1 MiB replacement still costs 3.44×/3.85× Whole and
  shrink 5.28×/5.12×. Neighbor isolation worsens; the 16 KiB shrink adds one stable page. This is
  size sensitivity, not workload acceptance, a production cap or custom-allocation authorization.
- **Native extent checkpoint (2026-09-13 14:02:33 UTC +0000):** After explicit authorization, the
  [allocator-only comparison](investigations/2026-09-13-property-body-extent-comparison.md) passed
  six native tests/36 extent rows; all 260 B-tree reference rows remain unchanged. Existing 0.7.2
  exposes no selected-node batch replacement/removal API; no fork was introduced. Contiguous
  1 MiB replacement writes 1048592 bytes versus 5559316 with 4 KiB chunks. Repeated hot shrink
  writes 260 bytes but still reads the full old 1 MiB value. Exact partition/index/owner oracles,
  pre-effect rejection, post-allocation retirement and nine paired reopens pass. The extra index
  doubles the raw short-body floor (two pages versus one); fragmentation and relocation can retain
  more capacity. These are native I/O results, not IC instructions, rollback/upgrade, directory
  atomicity or whole-row work bounds. The managed IC follow-up is recorded below; production
  selection remains pending.
- **Managed IC extent checkpoint (2026-09-13 20:50:20 UTC +0000):** The
  [91-case comparison](investigations/2026-09-13-property-body-extent-canbench.md) uses the same
  four-page variable-manager policy for one-region B-trees and two-region extents. All91 query
  cases pass; six native tests and296 numeric rows remain unchanged. Extent1MiB replacement/shrink
  costs3.15M/2.11M instructions versus Whole7.36M/5.86M and chunks26.71–32.31M/29.54–44.75M.
  Adverse results remain: Whole point-get1.43M beats extent read2.10M; allocation from already-
  coalesced space atF128 costs68.65K versus38.08–38.12K. Extent retains four extra physical pages
  in these fixtures (ten versus six small;26 versus22 large). Five initial grows add16 physical
  pages each; three also grow heap. Query measurement/local collection reload is not persistent-
  directory atomicity, Graph work admission or IC update rollback/upgrade proof. A separately
  reviewed integration design is justified, not production activation or a workload-weighted win.
- **Native PropertyStore checkpoint (2026-09-14 01:10:06 UTC +0000):** The
  [comparison](investigations/2026-09-13-property-store-native-comparison.md) implements actual
  8/14-byte keys, Graph Value codec, directory ownership and single-property moves for Direct,
  directory+B-tree4/16/64KiB and directory+custom extents. Ten tests pass;40 full stores produce
  2640 region rows, with all260+36 raw rows preserved. Candidates pre-admit selected bytes; Direct
  remains an unbounded admission reference. Small-directory isolation helps all candidates, but
  small mutations add reads and eager lanes retain backing. Chunk writes/moves amplify I/O;
  extent relocation retains holes/high-water, while B-tree deletion can also grow backing.
  These native counts do not establish IC speed or total work bounds. The separate managed
  full-store measurement follows below; frozen0357/0358 sources preserve historical provenance.
  Fine bucket/zero-fill/free-lookup optimizations remain deferred. No production format, whole-row
  safety, durable edge outcomes or bulk edge capability is selected.
- **Managed PropertyStore checkpoint (2026-09-17 22:24:02 UTC +0000):** The
  [full-store IC comparison](investigations/2026-09-17-property-store-managed-comparison.md) passes
  all210 full-store cases plus91 reused body controls. Actual keys, Value validation/codec,
  previous values and all regions are included. For edge keys, Direct ordinary1MiB get/replacement
  costs5.36M/9.85M instructions versus extent6.04M/10.25M; extent scalar insert/replace is about
  37%/60% more expensive. Extent large moves improve26.55M→12.11M, and small-neighbor isolation
  helps every separated candidate. Extent coalesced allocation costs118.36K versus Direct46.81K.
  Small physical totals are Direct6/chunks10/extent14 pages;1MiB point totals22/26/30. These are
  independently reserved capacities, not per-edge metadata or automatically wasted bytes. C64
  large move/deletion each grows four physical pages; extent relocation retains high-water.
  Native2640+296 rows reproduce. No production integration, total-work/whole-row proof,
  update/upgrade lifecycle validation or workload-weighted winner follows.
- **Impact:** Bulk-load edge SET remains unavailable. The selected direction needs bounded local
  selection/SET and durable terminal replay, not a cross-message edge-reference registry. Bulk-load
  edge inserts and vertex SET/REMOVE contracts are unchanged; ordinary GQL edge `SET` remains
  available. The tree, topology and inline-property read prerequisites do not expose a bulk operation.
- **Next decision:** Specify endpoint/policy semantics, local work/mutation/byte limits, exact
  positive/zero/rejection outcome representation, input-row versus matched-edge counts, and
  retention/Abort behavior before implementation. The existing exact-vertex seed and receipt
  contract do not represent this behavior as-is. No per-edge field or generation table is selected;
  recovery state belongs in the existing scalar journals and chunk receipts. Include actual
  serialized record/outbox/high-water measurements in the bounded validation plan.

### GAP-2026-09-11-001 — `PERCENTILE_CONT`/`PERCENTILE_DISC` fail closed on non-constant, NULL, or out-of-range fraction

- **Status:** Open — non-blocking follow-up recorded 2026-09-11 during the GQL-execution
  Unsupported-path survey. Fail-closed only; no wrong results. Low priority; a docs-only
  disposition is acceptable.
- **Owner:** Graph aggregate executor (`crates/graph/src/plan/query/aggregate.rs`).
- **Observed behavior:** Three error paths are reachable through grammatical queries:
  (1) `aggregate.rs:591` "aggregate: percentile fraction must be constant per group"
  for `PERCENTILE_CONT(x, <row-varying expr>)` such as `PERCENTILE_CONT(x, n.k)`;
  (2) `aggregate.rs:719` "aggregate: percentile fraction missing" for
  `PERCENTILE_CONT(x, NULL)` (fraction NULL on every row of a non-empty group);
  (3) `percentile_fraction_from_value` (`aggregate.rs:215-227`)
  `InvalidExpressionValue` for non-finite or out-of-`[0, 1]` fractions such as
  `PERCENTILE_CONT(x, 1.5)`. The constant-fraction requirement itself is SQL-consistent;
  the contract is simply undocumented.
- **Evidence:** source paths above; aggregate happy-path tests (`percentile_cont_median`
  et al.) cover no error path; no `percentile` mention in
  `design/gql/extension-syntax.md` or this ledger.
- **Expected or needed behavior:** The fraction contract (constant per group, non-NULL,
  finite, within `[0, 1]`) stated in `extension-syntax.md`, or error-path unit tests
  locking the three fail-closed arms.
- **Impact:** Users hitting these shapes get an execution error against an undocumented
  contract; nothing unsafe or misleading is returned.
- **Next decision:** Document the fraction contract in `extension-syntax.md` (smallest);
  add the three error-path unit tests only if the contract is ever relaxed.

### GAP-2026-09-11-002 — Cross-shard traversal fails closed on remote-vertex expand (federated scan limitation)

- **Status:** Open — non-blocking; single-shard execution (the only exercised topology)
  is unaffected. Recorded 2026-09-11 as one bundled entry during the GQL-execution
  Unsupported-path survey.
- **Owner:** Graph federation expand (`crates/graph/src/federation/expand.rs`) together
  with the streaming expand and WCOJ paths.
- **Observed behavior:** A traversal touching a vertex placed on another shard fails
  closed: `UnsupportedOp("cross-shard expand (remote vertex binding)")`
  (`federation/expand.rs:93`; sibling arms `:38,43,64,97,122`),
  `UnsupportedOp("Expand.var_len.remote")`
  (`plan/query/executor/scan/streaming.rs:570`), the limited-streaming remote-source
  arm (`streaming.rs:525`), and `WorstCaseOptimalJoin.remote_vertex` / `.var_len`
  (`plan/query/executor/wcoj.rs:21,154`). Relation to GAP-2026-08-25-003: that entry
  covers cross-shard *ordering* (concatenated sorted fragments); this entry covers
  cross-shard *traversal* (no remote-vertex resolution). Both stem from shard-local
  execution without a cross-shard fan-out.
- **Evidence:** source paths above; PocketIC fixtures are single-shard, so no E2E
  coverage exists for any of these arms.
- **Expected or needed behavior:** Either a remote-vertex resolution fan-out or an
  explicit documented federated-traversal limitation.
- **Impact:** Multi-shard deployments cannot traverse shard boundaries; queries fail
  with an explicit error rather than silently dropping rows.
- **Next decision:** Resolve inside the slice that introduces cross-shard resolution,
  if ever demanded; until then no action.

### GAP-2026-09-11-003 — Federated aggregate merge errors on all-NULL partials instead of yielding NULL/skip

- **Status:** Open — non-blocking, low priority. Fail-closed error direction (never a
  wrong result); single-shard execution unaffected; `Avg` excluded (already
  non-mergeable UnionRows). Recorded 2026-09-11 during the GQL-execution
  Unsupported-path survey.
- **Owner:** Router federation aggregate merge
  (`crates/router/src/federation/aggregate_merge.rs`).
- **Observed behavior:** For mergeable `COUNT`/`SUM`/`MIN`/`MAX`, when the
  first-ingested shard partial for a group is `Null` — e.g. grouped `SUM(x)` where one
  shard holds the group's rows but every value is NULL, so the Graph partial is Null —
  `merge_add_values` (`:345`) / `merge_extreme_value` (`:381`) return
  `Err("aggregate merge on null")` instead of skipping the NULL partial. All shards
  empty produces no rows and does not trigger.
- **Evidence:** source paths above; merge tests (`merge_aggregate_blobs_*`) cover valued
  partials only.
- **Expected or needed behavior:** SQL-consistent outcome: skip NULL partials and emit
  NULL when every partial is NULL.
- **Impact:** A rare multi-shard-only query error; the error direction is safe.
- **Next decision:** Smallest fix is "skip NULL partials, emit NULL when all partials
  are NULL" plus a two-shard unit test; open only on demand.

### GAP-2026-08-25-003 — Cross-shard global order for ordered-delivery and plain ORDER BY results (merge-aware union deferred)

- **Status:** Open — non-blocking follow-up recorded while landing ADR 0081 Slice A
  (2026-08-25). Not a regression introduced by that slice: Federation v1 already merged
  shard-local `ORDER BY` fragments by concatenation.
- **Owner:** Router federation merge (`crates/router/src/federation/merge.rs`
  `FederatedMergeMode::UnionRows` → `GqlWireRows::merge_optional_batch_blobs`)
  together with the future per-shard cursor-stream fan-out described in
  [ADR 0081 §Decision 3](../adr/0081-index-ordered-order-by-delivery.md).
- **Observed behavior:** Every current index read path delivers one globally ordered
  hit stream (native single posting map; single index canister per logical graph via
  `RouterIndexLookup::require_single_target`), so ADR 0081's R-way merge degenerates to
  identity and shards return ascending fragments. When a query fans out over multiple
  graph shards, the router concatenates those sorted fragments (`UnionRows`), so global
  cross-shard ordering is not guaranteed — both for the new ordered-delivery plans and,
  unchanged, for every pre-existing `Sort`/`TopK` plan executed per shard.
- **Evidence:** `design/gql/plan-format.md` §"IndexScan ordered-delivery intent";
  `crates/router/src/index_lookup.rs:315` (`require_single_target`);
  `crates/router/src/federation/merge.rs` UnionRows branch.
- **Expected or needed behavior:** When per-shard cursor streams exist, the federation
  bind boundary should run an R-way heap merge of the ascending streams (ADR 0081 §3)
  or, at the router, a merge-aware union for batches whose plans carry
  `IndexScan { ordered_by_sort }`.
- **Impact:** Multi-shard deployments cannot rely on router-level result ordering for
  `ORDER BY` queries; single-shard execution (the only mode exercised by native stores
  and current canister topologies) is unaffected.
- **Next decision:** Fold merge-aware union into the slice that introduces per-shard
  cursor fan-out; until then no action.
- **Status detail:** Blocked on the cursor-stream fan-out prerequisite; revisit when
  sharded postings land.

### GAP-2026-08-24-008 - RESOLVED: ELEMENT_ID projections need property-level READ grants (plan 0303 diagnosis; no Router defect)

- **Status:** Resolved (plan 0303, 2026-08-24). Root cause: the knowledge demo's grant
  surface lacked **property-level READ rows**. ELEMENT_ID-bearing projections demand
  `ReadProperty(label, property)` per projected key (`authz.rs:479-513`
  `require_vertex_scan_rows`; see also `labeled_scan_projection_contract`), which bare
  label-level `GRANT READ ... NODES <label>` does **not** cover (it lowers to the READ vertex
  row plus ReadProperty rows only for explicitly enumerated properties,
  `gql_grants.rs:413 resolve_property_ids`). Once brace-form
  `GRANT READ ... NODES Concept { name } TO PUBLIC` rows are applied, non-owner execution
  of `variable-length-reach` succeeds end-to-end (**7 rows**, verified live).
- **Owner:** demo grant surface (`demo/knowledge/scripts/apply-public-grants.sh`); walker
  semantics in `crates/router/src/authz.rs` confirmed correct (see
  `labeled_scan_projection_contract` test).
- **Observed behavior (final-state matrix, all ops freshly registered + published on a
  pristine demo-local network, canonical order):**

  | projection / shape | dev (non-owner) |
  | --- | --- |
  | RETURN 1 AS x (no graph) | PASS |
  | MATCH (n:Concept) RETURN n.name [AS name] [LIMIT n] | PASS (3 rows) |
  | MATCH (n:Concept) RETURN 1 AS x LIMIT 3 | PASS |
  | var-length incoming + DISTINCT, no ELEMENT_ID | PASS |
  | single-hop incoming + DISTINCT + ELEMENT_ID | DENY Forbidden |
  | var-length incoming + DISTINCT + ELEMENT_ID (= variable-length-reach) | DENY Forbidden, then PASS (7 rows) after property-level READ grant |

  Owner executes every variant via implicit-root tenancy bypass.
- **Mechanism:** ELEMENT_ID projections demand `ReadProperty(label, property)` rows; bare
  label-level READ grants do not emit those rows (`resolve_property_ids` enumerates only
  the statement's property list), so hydrated plans deny for non-tenants until a
  brace-form property-list READ grant is applied. No unattributed row is involved and no
  Router code change is required.
- **Expected or needed behavior:** published scenarios execute for arbitrary visitors once
  the PUBLIC surface covers match/traverse/read including per-property reads.
- **Evidence:** artifacts sections 13-14 in
  `design/investigations/artifacts/0296-knowledge-demo-bringup-evidence.txt`; probe scripts
  `scripts/register-probe.sh`, `scripts/direct-grant-probe.sh`,
  `scripts/apply-public-grants.sh` (all removed 2026-08-28 — superseded by
  `gleaph grants apply` and the grants policy file); contract-locking test
  `element_id_projection_demands_are_coverable_property_reads`.
- **Impact:** resolved for vertex scenarios. Two residual follow-ups: (a) RESOLVED by the
  plan 0306 diagnosis (2026-08-26) — this follow-up's original framing was a
  misattribution: requirement extraction on the current tree shows an edge `ELEMENT_ID(e)`
  projection adds no demand beyond the traversal row its label fact already covers, so
  citation-reach executes for PUBLIC once the brace-form property READ rows exist (contract
  test `edge_element_id_projection_demands_stay_attributed`; probe artifact
  `design/investigations/artifacts/0306-edge-element-id-demand-probe.txt`; no edge-property
  grant resource needed for element-id reads; runtime execution verified
  2026-08-26 only after GAP-2026-08-26-001 resolved the group element-id read);
  (b) OPEN — shortest-path currently hits
  IndexScan(no index client) under active sibling index/planner development (observation,
  not diagnosed here).
- **Fix direction:** none required in the walker. Demo-side resolution landed via
  apply-public-grants.sh. Optional future enhancement: let READ without a property list
  expand to all catalogued properties of the label.
- **Detection:** w1:p0 during plan 0296 quickstart; escalated by admin pane w1:pQ as P1.

### GAP-2026-08-24-009 — `ILIKE` evaluation deliberately ships without SQL LIKE wildcard semantics

- **Status:** Resolved — SQL LIKE slice A landed (LIKE new kind + ILIKE fold-then-match upgrade); ESCAPE clause landed as the follow-up below; LIKE prefix-index fusion landed separately (vertex + edge)
- **Follow-up (landed): `ESCAPE <char>` clause (2026-09-11, uncommitted slice):** standard SQL `x LIKE pattern ESCAPE char` / `x ILIKE pattern ESCAPE char` with an optional single-character escape replacing the default backslash (omitted clause keeps `\`; an explicit escape makes `\` ordinary). AST `StringPredicate.escape: Option<Box<Expr>>` (parser accepts the clause for LIKE/ILIKE only — `CONTAINS`/`STARTS WITH ... ESCAPE` fail closed at parse); type check warns on empty/multi-scalar literal escapes (strict mode → `TypeError`); execution resolves per-row (`NULL` escape → UNKNOWN, non-Text/non-single-scalar — including `$param` — → `IncomparableValues` fail-closed; ILIKE folds the escape with both operands, fold-collapse → fail-closed). Prefix fusion resolves the prefix with the static escape (`like_literal_prefix(pattern, escape)`); `$param`/bad-literal escapes stay residual-only. `NOT LIKE ... ESCAPE` rides the existing `negated` flag. This resolves the GQL `\%` spelling pain: `LIKE 'a#%b%' ESCAPE '#'` matches literal `a%b…` with no backslash doubling beyond GQL string rules. `$param` patterns stay non-fused (unchanged).
- **Owner:** `crates/graph/src/plan/expr_evaluator.rs` (`eval_string_predicate_expr` + `sql_like_match`) owns the runtime semantics
- **Resolution:** `StringPredicateKind::Like` added (AST tail variant, parser `eat_keyword("LIKE")`, `NOT LIKE` via the existing `negated` flag); `Like` runs SQL wildcard semantics over Unicode scalars (`%` any run, `_` one scalar, `\` escapes the next pattern scalar, trailing `\` literal); `ILike` folds both operands with `str::to_lowercase` and runs the same matcher. LIKE/ILIKE never fuse into an index anchor — residual filter only (`STARTS WITH` keeps the prefix-index path; planner regression `match_negated_or_other_string_predicates_never_anchor_and_stay_residual` extended with LIKE cases). Documented as a SQL-compat dialect extension in `design/gql/extension-syntax.md` (GQL core has no LIKE/ILIKE keywords).
- **Migration contract (breaking for `%`/`_` ILIKE users):** `ILIKE` previously implemented whole-string equality with literal `%`/`_` (tested contract). Those patterns are wildcards now: `'100%'` matches the `100` prefix, `'a_b'` matches any one-scalar middle. Match literals with `'100\%'` / `'a\_b'`. `LIKE` itself is new (non-breaking). The old literal-semantics test (`ilike_treats_percent_and_underscore_as_literals`) was rewritten to the wildcard contract (`like_and_ilike_apply_sql_wildcard_semantics`); no dual-path code remains.
- **Observed behavior (superseded):** Phase 1 string-predicate execution (commit introducing
  `ExprKind::StringPredicate` evaluation) implements `ILIKE` as plain Unicode case-insensitive
  whole-string equality: both operands are folded with `str::to_lowercase` and compared for exact
  equality. `%` and `_` are literal characters, not wildcards.
- **Contract basis:** No existing contract defined ILIKE semantics. Evidence surveyed:
  parser comment (`crates/gql/src/parser/expr.rs:1344`, "[NOT] CONTAINS, [NOT] ILIKE"), AST doc
  comment (`crates/gql/src/ast/expr.rs:145`), type-check constraint
  (`check_string_predicate` in `crates/gql/src/type_check/infer.rs:836`, which only requires
  string-compatible operands), and `ast/tests.rs`. No LIKE kind exists in the AST and no
  `%`/`_` handling exists anywhere in value evaluation, so the pre-production default applies:
  naive case-insensitive match with literal metacharacters. This complements the family:
  `STARTS WITH` / `ENDS WITH` / `CONTAINS` are case-sensitive positional matches, `ILIKE` is the
  case-insensitive whole-string match.
- **Impact:** Queries written with SQL-LIKE-style patterns (`'Ada%'`) will not match prefix-style;
  they require literal `%` in the data until a SQL LIKE feature defines wildcard semantics.
  Non-negated `STARTS WITH` TEXT-index pushdown (Phase 2) must preserve exactly these residual
  semantics for the non-indexed remainder.
- **Next decision (decided):** `LIKE` absorbed the wildcard grammar and `ILIKE` became its case-insensitive counterpart (fold-then-match); plain case-insensitive equality did not survive — see migration contract above.

### GAP-2026-08-24-010 — Edge-side `STARTS WITH` TEXT-index pushdown (vertex-only slice)

- **Status:** Resolved — edge-symmetric extension landed in the same slice family as the vertex
  side (fix: `feat(planner,graph,router)` commit `c8f60f894`, 2026-08-24; regression
  coverage listed below)
- **Owner:** Edge scan family: planner (`crates/gql-planner/src/planner/match_plan/path/filters.rs`
  edge fusion + `EdgeIndexScan` emission), executor
  (`crates/graph/src/plan/query/executor/scan/edge_index.rs`), Router seed extraction
  (`crates/router/src/seed.rs` edge anchor arms), and wire/explain/cost sync
- **Observed behavior:** The vertex slice fuses non-negated
  `v.prop STARTS WITH <Text literal | $param>` into `ScanValue::TextPrefix`
  (`AnchorSource::PropertyPrefix`, `IndexAnchor::Prefix` seeds) over range-indexed vertex
  properties. Edge properties had no symmetric path: `-[e:R WHERE e.prop STARTS WITH 'x']->`
  stayed fully residual even when `e.prop` has an ordered edge index.
- **Resolution:** `EdgeFilterFusion.indexed_prefix` fuses after equality → IN → range and keeps the
  predicate residual; the leading bound chain lowers to
  `PlanOp::EdgeIndexScan { value: TextPrefix, cmp: Eq }`; eligibility flows through
  `has_indexed_edge_bound`; executor intercepts TextPrefix before equality dispatch and derives the
  shared `text_prefix_range_bounds` interval (`lookup_edge_range_local`, Null pattern binds no rows,
  non-TEXT resolved pattern falls back to the canonical superset); Router extracts
  `IndexAnchor::EdgePrefix(EdgePrefixSeedProbe)` at all three edge extraction sites via
  `edge_prefix_anchor` (reusing `resolve_edge_equal_seed_context`) and executes it through
  `lookup_edge_range_wires`. Landing this slice exposed and fixed the filter-pushdown stale-index
  defect recorded as GAP-2026-08-24-011. Regression coverage: planner edge prefix matrix,
  exact-Between-bytes mock test, missing-parameter / Null / non-TEXT contracts, native e2e boundary
  row sets with red-proof, Router extraction + execution harness tests.
- **Contract basis:** Same shape as GAP-2026-08-24-002 (edge IN-list symmetry): the interval
  primitive exists (`PostingRangeRequest::Between` via `lookup_edge_range`, used by edge range
  anchors since GAP-2026-07-29-003); bound derivation reuses
  `text_prefix_range_bounds(_for_encoded_key)` from `crates/gql/src/value_index_key.rs`; no new
  key-encoding knowledge outside that file.

### GAP-2026-08-24-011 — Filter-pushdown applied index moves sequentially, dragging filters before their producer

- **Status:** Resolved — fixed in the same slice that discovered it (fix:
  `feat(planner,graph,router)` commit `c8f60f894`, 2026-08-24)
- **Owner:** `crates/gql-planner/src/pushdown/filter.rs` (`apply_filter_pushdown`)
- **Observed behavior:** With two or more PropertyFilter moves in one pass, `(from, to)` pairs were
  applied via sequential `remove`/`insert`. The first insertion shifts every later index, so a
  second move addressed a stale position and dragged an unrelated filter across a producer op.
  Concrete repro on main before the fix: any leading one-sided range edge anchor with a path-level
  WHERE residual — `[EdgeIndexScan{Ge}, bind(a,b), PF(IsLabeled b), PF(IsLabeled a),
  PF(e.weight >= 7)]` reordered to `[scan, PF(residual), PF(IsLabeled b), bind, PF(IsLabeled a)]`,
  executing `MATCH (a)-[e:R]->(b) WHERE e.weight >= 7 RETURN b` failed with
  `MissingBinding { variable: "b" }`. Equality/IN-list anchors masked the defect because their fused
  predicates are removed from residuals, leaving only one move per pass.
- **Impact:** Silent wrong plans (execution errors or, worse, filters evaluated against unbound
  variables) for multi-move pushdown passes; blocked every non-equality edge pushdown e2e.
- **Resolution:** Moves are now queued per anchor (`to - 1`, which is always an unmoved producer op)
  and the op list is rebuilt in one pass against original positions; relative order of same-anchor
  moves follows original positions. Regression tests:
  `filter_pushdown_never_moves_filters_before_their_producer`
  (planner unit invariant) and `parsed_edge_range_path_level_where_binds_projected_rows`
  (native e2e through the shared leading-edge path).
- **Next decision:** None open; if future passes adopt positional moves again, they must rebuild
  rather than remove/insert incrementally.

### GAP-2026-08-24-005 — ADR 0074 grant statements cannot address hyphenated prepared-query names

- **Status:** Resolved 2026-09-08 (uncommitted working-tree fix; no commit per primary-owns-commits) — `Parser::expect_prepared_query_name` (helpers.rs) joins bare `Ident (- segment)*` with `Int`/`BigInt` digit-led segments plus span-adjacent tail glue, and keeps the exact `QuotedIdent` arm; GRANT/REVOKE `PREPARED QUERY` plus `EXPLAIN AUTHORIZATION FOR PREPARED QUERY` share the production. Previously blocked `GRANT EXECUTE ON PREPARED QUERY <name> TO PUBLIC` for every hyphenated prepared operation (ADR 0061 names are `[a-z][a-z0-9-]*`, so most real names, e.g. the knowledge demo's `variable-length-reach`, were affected)
  **Live-reproduction + red-test addendum (2026-08-24, plan 0296 quickstart):** the rejection is
  no longer PocketIC-only. On a real demo-local network, `gleaph prepared publish citation-reach`
  fails with `InvalidArgument("parse error: expected 'TO', got '-'")` (Router wasm built from the
  current tree minutes earlier — not a stale-artifact symptom), and the tree's own
  `grant_parser_tests` under `#[cfg(feature = "gleaph")]`
  (`crates/gql/src/parser/statement.rs:2010`) are red on main:
  `parse_grant_execute_on_prepared_query_to_public_and_principal` and
  `parse_revoke_execute_on_prepared_query_mirrors_grant` both panic with the same lexer error.
  `statement.rs` sits in the owning pane's uncommitted edit set. **Operational workaround until
  this fix (retained as the canonical CLI spelling):** emit the op name as a double-quoted identifier — the lexer produces
  `Token::QuotedIdent` (`crates/gql/src/lexer.rs:628`) and grant-target `expect_ident` accepts it
  (`crates/gql/src/parser/helpers.rs:232`) — adopted by the CLI's
  `prepared::publication_statement` with parser-acceptance unit tests.
- **Owner:** `crates/gql` authorization-statement lexer/parser (`parse_grant_statement` →
  `expect_ident`; hyphen lexes as minus, not part of an unquoted identifier)
- **Merged from the removed duplicate entry (plan 0306 sync, 2026-08-26):** underscore names
  (`cli_run_tag`) fail CLI-side kebab-case validation instead, so no publishable spelling
  exists for them today; `prepare` registration and CLI name validation both accept hyphenated
  names while the grant statement rejects them, and parser tests assert `find-users` parses —
  a conflict with the runtime lexer behavior to reconcile before fixing. Resolution options
  recorded there: accept ADR 0061 prepared-name syntax for the PREPARED QUERY target (hyphenated
  or quoted identifiers), or restrict prepared names to one grammar shared by both sides; a
  lexer extension needs an ADR 0074 §5 amendment note. No correctness gap; default-deny stays
  intact.
  **Resolution 2026-09-08 (option A, parser-only, no lexer change):** `Parser::expect_prepared_query_name`
  accepts the ADR 0061 spelling in the three `PREPARED QUERY` positions (GRANT `TO`, REVOKE `FROM`,
  EXPLAIN AUTHORIZATION name + `BY PRINCIPAL` tail unchanged); full charset validation stays with
  prepared registration/Router resolution. Regression: `parse_prepared_query_name_accepts_bare_kebab_digits_and_quoted`
  (`variable-length-reach`, `phase-2-rollout`, `op-2fa`, `find-to` terminator guard, quoted `find-users`,
  EXPLAIN bare + BY PRINCIPAL). Validation: `gleaph-gql --features gleaph --lib` 567 passed / 0 failed,
  default `--lib` 524 passed / 0 failed, `cargo fmt -p gleaph-gql -- --check` clean, `git diff --check` clean.
  The previously-red `parse_grant_execute_on_prepared_query_to_public_and_principal` and
  `parse_revoke_execute_on_prepared_query_mirrors_grant` pass unmodified (no test weakening).

### GAP-2026-08-24-007 — Committed HEAD does not build `gleaph-router`: `auth::require_admin` callers landed before their auth definitions

- **Status:** Open — window is green only while the owning pane's uncommitted WIP stays in the
  tree; resolution owned by the ADR 0075 stream (w1:pT, notified 2026-08-24)
- **Owner:** `crates/auth/src/lib.rs` (missing definitions) vs `0d9f58421`/`73d4b3737`/`e9159938b`
  (router-side callers: `facade/store/catalogs.rs`, `idempotency.rs`, …)
- **Observed behavior:** An isolated worktree at `5a8e6dc8a` (current main HEAD at detection time)
  fails to compile `gleaph-router --lib` with `E0425 cannot find function require_admin in module
  auth` (multiple sites). The main working tree passes (`951 passed / 0 failed`) because the
  uncommitted auth-side changes of the same workstream are still present.
- **Expected or needed behavior:** Every commit on main must build standalone. The auth-side
  definitions (or an equivalent refactor of the callers) must land in the same push that lands the
  callers. Same failure mode as the `8e392c127` half-committed `edge_properties.rs` incident.
- **Evidence:** `git show 5a8e6dc8a:crates/router/src/facade/store/catalogs.rs | rg require_admin`
  vs absent definition in `git show 5a8e6dc8a:crates/auth/src/lib.rs`; isolated-worktree build log.
- **Impact:** Bisect over this range reports false positives; any fresh clone/worktree at these
  commits cannot run router tests.
- **Detection:** w1:pQ during edge IN-list slice verification (Phase 2 final gate), 2026-08-24.

### GAP-2026-08-24-006 — Pure-CLI platform bring-up fails on the Gleaph-owned launcher network

- **Status:** Fully Resolved (2026-08-26). (a) operator tier + CLI deploy, (b) daemonization,
  and (c) the Provision init/grant model refinement (below) all landed; `gleaph identity new
  dev && gleaph network start -d` completes end to end (Account/Provision deploy + account
  auto-registration) on a Gleaph-owned launcher network; `gleaph-operator grant upsert`
  verified against the live network
- **Owner:** `crates/cli/src/network.rs` (launcher lifecycle, management-canister transport)
  and the launcher's HTTP gateway contract (`icp-cli-network-launcher` v15.0.0);
  Account/Provision deployment flow in `network::start`
- **Observed behavior:** Two independent failures, each reproduced twice on 2026-08-24
  (macOS arm64, launcher v15.0.0):
  1. *Management canister updates are rejected by the launcher gateway.* After a successful
     launch (root-key fetch over `http://localhost:8000/api/v2/...` succeeds), the first
     management-canister update call fails:
     `update create_canister: The replica returned an HTTP Error: Http Error: status 400 Bad Request, content type "text/plain; charset=utf-8", content: error: canister_not_found details: The specified canister does not exist.`
     The read-side (`fetch_root_key`, i.e. `read_state` on `aaaaa-aa`) reaches the replica,
     so the network is up — the rejection is specific to update calls targeting
     `aaaaa-aa`. Because `create_canister` is exactly how `network start` deploys
     Account/Provision (`deploy_canister` → `install_canister`), no canister is deployed
     and the mapping file is never written.
  2. *The launched network dies with the parent CLI process.* In both runs the status file
     `$TMPDIR/gleaph-local-status/status.json` and the gateway listener on port 8000 were
     gone seconds after `gleaph network start -d …` exited. `-d/--background` detaches the
     child's stdio but does not setsid it, so the launcher (and its PocketIC process chain)
     does not survive the CLI session that started it.
- **Expected or needed behavior:** `gleaph identity new dev && gleaph network start -d …`
  followed by data-plane commands must work with only a clean checkout and Rust toolchain —
  per the plan 0296 quickstart contract — including management-canister deployment of
  Account/Provision from local wasm artifacts and a launcher that persists independently of
  the starting process.
- **Evidence:** `/tmp/gq-netstart2.log` through `/tmp/gq-netstart4.log` in the 0296 slice
  session; fix for the related staging defect (v15 tarballs nest the binaries under a
  versioned directory) landed as `normalize_extracted_launcher` in `crates/cli/src/network.rs`
  with unit tests (`normalize_stages_nested_launcher_and_companion`,
  `normalize_is_idempotent_when_flat_binary_already_cached`,
  `normalize_fails_closed_when_the_archive_has_no_launcher`).
- **Impact:** The knowledge-demo quickstart cannot use the pure-CLI bring-up path; it runs
  against an icp-cli managed network with a directly deployed platform instead (see plan
  0296 revision 3). No data-plane or authorization impact.
- **Next decision:** Future slice "pure-CLI bring-up": (a) determine whether the launcher
  gateway can route management-canister updates (or whether Account/Provision must be
  installed via a different entrypoint), (b) daemonize/supervise the launcher chain so `-d`
  outlives the CLI (setsid + optional supervisor), (c) then revisit whether lazy issuance
  plus provisioned topology completes without catalog-upload plumbing (ADR 0036 has no CLI
  upload surface today). Decided 2026-08-26 ([ADR 0087](adr/0087-wasm-ingestion-operations-model.md)):
  the catalog-upload surface lives in `gleaph-operator` over a shared ingestion client library,
  not the developer CLI; pure-CLI bring-up seeds the local catalog through that library.

  **(a) resolved 2026-08-26.** The launcher gateway *can* route management-canister updates;
  the two observed failures were operator-side effective-canister-id routing, not a gateway
  limitation. `gleaph-operator bootstrap deploy` now (1) uses `provisional_create_canister_with_cycles`
  for local/PocketIC endpoints (ingress-level `create_canister` has no derived-effective-id
  routing there), and (2) sets the effective canister id per call: the target canister for
  `upload_chunk`/`install_chunked_code`/`stop`/`start`/`canister_status`, and the network's
  default effective canister id (read from the `/_/topology` endpoint, the same source dfx's
  `dfx info default-effective-canister-id` uses) for the provisional create, whose response
  certification requires the effective id to fall within the target subnet's canister ranges.
  Verified end to end against a locally launched launcher network: create → chunked install →
  stop → upgrade → start with exact module-hash match. (b) daemonization remains open.

  **(b) resolved 2026-08-26.** `spawn_launcher` now calls `setsid()` (via `Command::pre_exec`)
  in background mode, creating a new session so the launcher (and its PocketIC process chain)
  survives the CLI's exit and a terminal close. Verified: `gleaph network start -d` run inside a
  pseudo-terminal leaves the launcher alive after the pty closes (previously it died on SIGHUP).

  **Remaining: CLI `network start` deploy still fails.** The operator's bootstrap tier is fixed,
  but `crates/cli/src/network.rs` `deploy_canister` still calls `create_canister` (no
  `provisional_create_canister_with_cycles`, no per-call effective canister id), so the
  pure-CLI bring-up deploy rejects with `canister_not_found` on the launcher network. The same
  GAP-2026-08-24-006(a) fix must be applied to the CLI's `RemoteTransport` management calls
  before `gleaph network start` completes.

  **Resolved 2026-08-27 — CLI deploy + Provision grant-model refinement (c).**
  Two changes completed the pure-CLI bring-up:

  1. *CLI deploy:* `RemoteTransport` mirrors the operator fix — local deploys go through
     `provisional_create_canister_with_cycles` with per-call effective canister id (target
     canister for install, `/_/topology` default for the provisional create). Additionally the
     third launcher-start failure surfaced and was fixed: a reused, incomplete PocketIC state
     dir panicked the launcher at startup (`nonblocking.rs: The state of subnet … is
     incomplete`); `spawn_launcher` now removes the state dir before each start (the official
     icp-cli pattern), redirects stdout/stderr to `launcher.stdout.log`/`launcher.stderr.log`,
     and detects premature launcher exit, surfacing the stderr tail. It also refuses to start
     a second launcher while one is alive (`gleaph network stop` first).

  2. *Provision trust-model redesign (deployment grants).* The per-deployment
     `DeploymentBinding` (router/governance/bootstrap principals + binding_version) is replaced
     by a single set of authorized issuers:

     - `DeploymentTrustStore` → `DeploymentGrantStore` = `StableBTreeSet<Principal>`; the set
       holds principals authorized to request issuance. `admin_install_deployment_binding` →
       `upsert_deployment_grant` (governance-only, idempotent; `BootstrapAuthAction::Upsert`).
     - `DeploymentBinding.router_principal/governance_principal/bootstrap_principal/binding_version`
       and the `complete_bootstrap` handover are all removed: deployment_id was a map key
       duplicated in the value, binding_version was never read for logic, and the bootstrap
       handover existed only to break a circularity that the set model dissolves. Each deploy is
       independent — the account (via the Account canister) issues the first Router; the issued
       Router is auto-granted at install time and issues graph resources; no transition concept.
     - `ProvisionInitArgs` = `{ governance_principal }` (the only authority established at
       init; grants are seeded per issuer afterwards). The CLI `network start` passes its
       session principal as the governance authority.
     - Envelope auth is set membership (`caller ∈ grants`); the deployment_id in an envelope is
       data (the Account canister issues under the user's account principal, the Router under
       its own). Authorization never depended on the deployment id again.
     - Operator CLI `binding install` → `grant upsert <ISSUER>`; init-args JSON is
       `{"governance_principal": "…"}`.

     Verified: provision (110 unit tests), router (1043), operator (40), cli (142), account,
     and the PocketIC E2E suite for the provision-affected targets (adr0035 callable
     endpoints/outbound, adr0068 deployment/issuance, adr0087 bootstrap tier/operator
     ingestion, text_index_provisioning, adr0070 fixture family 1). Known pre-existing E2E
     failures unrelated to this change: adr0070 fixture 2 (`NotFound("graph context")` — RBAC
     gate precedes the catalog-DDL path), adr0034 (vector stream WIP), router_gql_query
     barrier test (ADR 0029 query/update export mismatch).

### GAP-2026-08-24-002 — Edge-index anchors do not accept `ScanValue::InList` (symmetric extension of the vertex IN-list anchor)

- **Status:** Resolved (this commit)
- **Owner:** `gleaph-gql-planner` (`EdgeFilterFusion`, `parse_edge_var_property_equality`) +
  `gleaph-graph` executor (`execute_edge_index_scan`) + Router seed lowering (`edge_equal_anchor`)
- **Observed behavior:** The IN-list anchor slice (commit series ending with the vertex IN union)
  lowers only vertex predicates (`WHERE v.<indexed-prop> IN […]`) into `ScanValue::InList` union
  point probes. An edge predicate `WHERE e.<indexed-prop> IN […]` keeps its single-value
  `indexed_equality` path and evaluates the list as an ordinary residual filter.
- **Expected or needed behavior:** Symmetric with the vertex anchor: a scannable non-negated edge
  IN list could probe each element against the edge property index and union the postings.
- **Evidence:** `crates/gql-planner/src/planner/match_plan/path/filters.rs` (vertex lowering),
  `crates/router/src/seed.rs` (`IndexAnchor::EqualUnion`, edge arms unchanged).
- **Impact:** Edge IN queries stay correct but scan more incident edges than necessary; no
  correctness gap.
- **Next decision:** Whether edge equality fusion should reuse `ScanValue::InList` directly or add
  an edge-specific multi-probe shape that also carries the label/direction subset rule.
- **Resolution (this commit):** Reused `ScanValue::InList` directly — no new wire or planner
  shapes. The planner fuses a scannable non-negated edge IN conjunct through the existing
  `EdgeFilterFusion.indexed_equality` slot (`parse_edge_var_property_inlist` + shared
  `find_first_indexed_edge_bound_in_conjunctions`; every equality bound wins before any IN fusion,
  NOT IN never anchors, and the fused bound is removed from residuals exactly like fused equality).
  All three executor consumers of `indexed_edge_equality` gained per-element probe unions
  (`execute_edge_index_scan`, `edge_equality_stream_filter`, `expand_candidates_via_equality_index`;
  Null elements contribute no probe, missing parameters fail closed, duplicate list elements
  deduplicate on edge identity, `cmp != Eq` stays fail-closed). The Router extracts
  `IndexAnchor::EdgeEqualUnion` in all three seed-extraction sites with per-probe ADR 0012
  wire-label subsets and global `(shard, owner, label, slot)` deduplication; `resolve_scan_value`'s
  InList arm remains a malformed-plan rejection because unions expand before resolution. Owning
  tests: `crates/gql-planner/tests/planner_tests.rs::match_edge_*inlist*`,
  `crates/graph/src/plan/query/executor/scan/tests.rs::{executes_edge_inlist_*,edge_inlist_*,indexed_edge_inlist_queries_end_to_end_match_equality_semantics}`,
  `crates/router/src/seed.rs::tests::edge_inlist_index_scan_extracts_edge_equal_union_with_per_element_probes`.

### GAP-2026-08-24-003 — Cypher bracket-form `IN […]` fails to parse when `sql-compat` is enabled

- **Status:** Resolved — one-token opener lookahead in the sql-compat arm; fix commit
  `37d59c7f9`, owning regression tests
  `gql::parser_tests::in_list_combined_build_serves_both_list_forms`,
  `in_list_is_rejected_in_the_default_build` (default cell),
  `in_list_cypher_only_build_serves_both_list_forms` (cypher-only cell),
  `in_list_sql_compat_only_build_serves_paren_and_rejects_bracket` (sql-compat-only cell), and the
  combined-build graph e2e
  `parsed_bracket_in_list_returns_matching_rows_under_combined_dialects`
- **Owner:** `gleaph-gql` parser (`crates/gql/src/parser/expr.rs`, `IN` predicate arms)
- **Observed behavior:** With both `cypher` and `sql-compat` features enabled, `expr IN [v1, v2]`
  failed with `expected '(', got '['`. The `sql-compat` arm ran first and unconditionally required
  `(` after `IN`; its comment claimed precedence "keeps behavior unchanged", but it aborted parsing
  instead of falling through to the cypher arm's bracket form.
- **Fix:** The sql-compat arm now claims the `IN` predicate only when the token after `IN` is `(`
  (`peek_ahead` off the same keyword test that decides the `IN`/`NOT IN` shape). A non-`(` opener
  falls through untouched: in combined builds the cypher bracket grammar serves it; in a
  sql-compat-only build no arm claims it and the query still fails as a parse error. Single-feature
  and default configurations keep their exact acceptance sets; only the error text for a rejected
  bracket form under sql-compat-only shifts from "expected '('" to the statement-level leftover
  token error.
- **Evidence:** Before the fix, `in_list_combined_build_serves_both_list_forms` failed at parse;
  with the fix all four feature combinations pass their pinned cells
  (`cargo test -p gleaph-gql [features] in_list`: default 1, cypher 5, sql-compat 3,
  `cypher,sql-compat` 7) plus the combined-build e2e returning rows for
  `WHERE p.age IN [5, 7]`.
- **Impact:** Bracket-form IN queries parse and execute under combined dialect builds; feature
  combinations no longer diverge on `IN […]`. Default canister dialect unaffected.

### GAP-2026-08-24-004 — Endpoint property projection on `EdgeBindEndpoints` breaks trailing `IsLabeled` filters

- **Status:** Resolved — planner-side entity-use veto; fix commit `0cade11a8`,
  owning regression tests
  `gql-planner::property_projection::tests::is_labeled_residual_keeps_bind_endpoint_slot_full`,
  `planner_tests::trailing_is_labeled_residual_keeps_edge_bind_endpoint_vertex_binding` (+ near,
  positive-control, and cypher/entity variants), and graph e2e
  `parsed_is_labeled_residual_with_projected_far_endpoint_returns_matching_rows`
- **Owner:** planner late-projection pushdown (`crates/gql-planner/src/property_projection.rs`)
  exclusively; executor binding layout and the fail-closed non-Vertex `IsLabeled` semantics are
  unchanged
- **Observed behavior:** For a parsed leading-edge-anchor query whose projection touches an
  endpoint property (for example `MATCH (a:X)-[e:R WHERE e.weight = 5]->(b:Y) RETURN b.name`),
  the planner pushed `far_property_projection = ["name"]` into `EdgeBindEndpoints` (late
  projection) and still emitted `PropertyFilter(IsLabeled(b))` after it. Execution bound `b` to a
  projected `PlanBinding::Value(Record)` (`vertex_binding_for_projection`, `Some(props)` arm), so
  the subsequent `IsLabeled(b)` check no longer saw a vertex binding and dropped **every** row.
  Root cause: the pushdown veto (`must_remain_vertex`) only saw structural uses (SEARCH bindings,
  traversal sources); expression-level entity use was invisible because `var_used_as_non_property_receiver`
  short-circuited `ExprKind::IsLabeled` to `false`, so the RETURN-side `PropertyAccess(b, name)`
  alone decided the projection.
- **Fix:** Added a third downstream collector, `collect_entity_used_bindings`, which walks the
  suffix expressions and records every `Variable(v)` occurrence except direct `PropertyAccess`
  subjects, and unioned it into `must_remain_vertex`. All scan/expand/bind slots now veto to
  `ScanProjectionPatch::FullProperties` while any entity-level use remains downstream — including
  `expand_edge`, which previously ran its inference without any identity veto. Over-veto is
  conservative (full hydration = pre-pushdown behavior), never changes results.
- **Evidence:** Before the fix, the e2e regression test returned 0 rows for the projected query
  with the unprojected whole-variable twin returning 1; after the vetoes, both return the matching
  row. Planner-level repro pinned at plan level: `far_property_projection` stayed `Some(["name"])`
  with a trailing `PropertyFilter(IsLabeled(b))` before the fix, `None` after.
- **Impact:** Property-level projections over labeled endpoints now execute correctly for anchored
  edge scans; only queries mixing endpoint property reads with entity-level use of the same
  variable lose the projection optimization (correct results either way). Whole-variable
  projections were never affected.

### GAP-2026-08-24-001 — Two stale router lib fixtures surfaced on the suite's first full run after the 8e392c127→ac380c4bb build-broken window

- **Status:** Resolved (this commit)
- **Severity:** P3 test fixture — the fail-closed verification logic was correct in both cases; no
  production defect
- **Owner:** `gleaph-router` native unit-test fixtures (`facade/stable/indexed_catalog.rs`,
  `vector_sync.rs`)
- **Observed behavior:** Once GAP-2026-08-23-001's ic0-context deferral unblocked
  `cargo test -p gleaph-router --lib` on this host, the first clean full run failed 3 of 937 tests,
  all stale fixtures rather than regressions:
  1. `facade::stable::indexed_catalog::tests::maintenance_projection_preserves_namespace_phase_and_epoch`
     asserted `vertex_indexes.len() == 1` but observed 0. Since
     `8f3f5c1da fix(index): fail closed on inconsistent vertex leaf identities`, the catalog projection
     requires `vertex_leaf_projection(graph_id, property_id)` to resolve the reverse property name and
     omits whole rows it cannot resolve; the fixture inserted the physical-101 Active Vertex record with
     property id 5 without interning any reverse name, so the row was (correctly) omitted.
  2. `vector_sync::tests::frontier_response_loss_retains_exact_marker_snapshot` expected the publisher to
     observe `(Principal([1;29]), ShardId(2), 52)` but saw no call. Since
     `128940f51 feat(router): publish markerless vector lane frontiers`, frontier publication is
     catalog-driven (`run_recovery_pass` → `run_catalog_frontier_pass` →
     `graph_catalog::scan_attached_vector_lane`, which reads `ROUTER_SHARDS`); the test seeded only outbox
     markers with no attached lane, so `publish_router_frontier` was unreachable. The third failure,
     `resolved_rows_transition_to_awaiting_frontier_before_publish`, was lock-poison cascade from item 2's panic.
- **Expected or needed behavior:** Fixtures must satisfy the current contracts without weakening any
  assertion. Both verifications are intended fail-closed behavior: an unresolvable leaf identity omits the
  catalog row whole instead of flattening it, and frontier publication requires a catalog-attached lane.
- **Fix:** Item 1 interns the Active Vertex leaf's flat property name through the module's existing
  `intern_property` helper and passes the returned id into the fixture record; the assertions key on
  `physical_index_id`/`catalog_epoch` so they are property-id agnostic. Preparing/Aborting rows need no
  name because `maintenance_phase` excludes them before projection, and the Building/Sealing rows are
  Edge-kind. Item 2 adds a local `attach_catalog_lanes` helper in the vector_sync tests module (isomorphic
  to the outbox-side fixture) registering exactly the `(Principal([1;29]), ShardId(2))` lane that
  `intent()` targets; `clear_for_test()` already resets `FRONTIER_CATALOG_CURSOR`/`FRONTIER_LANE_PROGRESS`.
  No assertion or verification path changed.
- **Owning tests:** the two named tests plus the de-poisoned
  `vector_sync::tests::resolved_rows_transition_to_awaiting_frontier_before_publish`; full
  `cargo test -p gleaph-router --lib` green (937 passed).

### GAP-2026-08-23-004 — Router canister wasm exceeds the IC code-section limit once the ADR 0074 slice 2a grant grammar is linked

- **Status:** Closed (2026-08-23) via prerequisite plan `0288` (router wasm budget recovery).
  Decision: build-path unification plus release-profile levers instead of source-level
  slimming. Every Gleaph canister build path (root `icp.yaml` script adapters,
  `scripts/deploy-demo-local.sh`'s instrumented router, the PocketIC fixture build,
  `scripts/check-codegen-local-e2e.sh`) now runs one shared post-processing entry point,
  `scripts/postprocess-canister-wasm.sh` (`ic-wasm` metadata insertion → candid extraction →
  `ic-wasm shrink --keep-name-section`; the name section stays kept deliberately — it is a
  custom section and does not count against the code-section limit), and `[profile.release]`
  gained `lto = "fat"` + `codegen-units = 1`. Binaryen `wasm-opt` was not needed.
- **Observed behavior:** `cargo test -p gleaph-pocket-ic-tests` fails every suite with
  `Wasm module code section size of 12615453 exceeds the maximum allowed size of 12582912`
  during Router install (PocketIC enforces the mainnet limit). Measured code sections of
  `gleaph_router.wasm` (wasm64, release, E2E features): HEAD + concurrent-session overlay
  without slice 2a = **11,942,769**; with slice 2a = **12,123,923** (+181,154); six-package
  feature-unified E2E build = **12,615,455** (+32,543 over the limit).
- **Attribution evidence:** name-section diff of the two single-package binaries shows the
  largest genuinely new linkage is `data_encoding` decode machinery (**+61,772 bytes**, 19 → 49
  functions), introduced by the first production use of `candid::Principal::from_text` in
  `crates/router/src/gql_grants.rs::bind_subject` (all pre-existing `from_text` call sites are
  test-only; `to_text` was already linked via provisioning `deployment_id`). An isolated
  experiment replacing `from_text` with `from_slice` shrinks the binary by 76,068 bytes,
  confirming the attribution; the remaining ~105KB of delta is inlining-budget churn spread
  across unrelated shared symbols (growth and shrinkage nearly cancel: +1.87 MB / −1.87 MB).
- **Needed behavior:** ADR 0074 §5 requires the integration layer to bind `PRINCIPAL <literal>`
  subjects to identities, which requires principal-text decoding in the Router; the IC
  install-time code-section ceiling is hard on both mainnet and PocketIC.
- **Owner:** Router canister build composition (`crates/router`, workspace release/wasm64
  profile, `crates/pocket-ic-tests/build.rs`). The principal-text contract itself stays owned
  by `candid`; a hand-rolled base32/CRC decoder in the Router was considered and rejected as a
  duplication of candid's identity-validation logic. Resolution adds
  `scripts/postprocess-canister-wasm.sh` and the workspace `[profile.release]` as the owning
  size-budget surfaces.
- **Impact:** ~~all PocketIC E2E suites are blocked until the Router drops ≥33 KB below the
  current footprint~~ resolved: see resolution evidence below; plan `0287` todo
  `pocketic-lifecycle-tests` is unblocked.
- **Resolution evidence (2026-08-23, plan `0288`; limit 12,582,912 code-section bytes,
  measured with `scripts/wasm-code-section-size.py`):**
  - PocketIC fixture path (six-package feature-unified wasm64+SIMD release), isolated levers:
    raw cargo output **12,123,923** → + `[profile.release] lto="fat", codegen-units=1`
    **10,689,428** (−1,434,495) → + shared post-processing **9,634,923** (−1,054,505).
    Headroom vs the limit: **2,947,989 bytes (~2.81 MiB)**, ~11× the required 256 KiB.
    Today's pre-change reproduction measures lower than the recorded failing build
    (nightly `c656540d6 2026-08-21` and/or concurrent-session overlay churn; not bisected —
    applying the same measured deltas to the recorded 12,615,455 footprint estimates ≈10.13 MB,
    still ≥2.4 MiB under).
  - Per-path router code sections after both levers: deploy (`icp build gleaph-router`)
    **9,597,708**; demo instrumented (`batch-instr-log`, feature-guard grep verified)
    **9,614,992**; PocketIC fixture **9,634,923** — all paths installable with ≥2.8 MiB margin.
  - Other canisters strictly smaller than before on every path (PocketIC fixture code
    sections, before → after): graph 7,405,381→6,094,339; graph-index 2,098,383→1,744,550;
    vector 1,890,704→1,512,047; account 973,145→805,582; provision 1,677,761→1,382,074.
  - Blocked validations rerun green: `smoke` 1/1, `adr0074_auth_ingress_walk` 1/1,
    `adr0074_grant_grammar` 4/4 (first-ever execution exposed a self-contradictory hardcoded
    subject expectation in the suite's own `expect_grant_summary` helper — it asserted `Public`
    while its callers grant to `PRINCIPAL` and assert the principal subject; the helper now
    takes the expected subject, no canister-side change was needed, stored rows were correct).
- **Next decision:** none (closed). Standing watch: cross-nightly rebuild drift is real
  (~±500 KB observed); keep the ≥256 KiB headroom check per canister when touching canister
  features or toolchain versions, measured with `scripts/wasm-code-section-size.py`.

### GAP-2026-08-23-003 — ForceAtlas2 equilibrium rests below the rendered node diameter once nodes are world-sized

- **Status:** Closed (2026-08-23). Decision: recalibrate the repulsion-scaling
  default so the force balance itself rests outside marker overlap
  (`scaling = 432`, derived from `d* = (S·m²)^{1/3}` with mass 2 pairs equaling
  the canonical 12-unit node diameter), documented in DESIGN.md §15.1. A hard
  pairwise separation floor was prototyped first and rejected: dense topologies
  make it unsatisfiable (hub/256 leaf ring spacing ~1.2 units against a 12-unit
  floor), so the constraint fights the force law forever and the layout never
  settles. Residual limitation, accepted as capacity physics rather than a
  defect: leaves around a massive hub still rest partially overlapped
  (~9 units apart at hub/256) because their packing density exceeds what any
  spacing floor allows — matching reference FA2 behavior. If overview-zoom
  readability of such dense hubs ever matters, revisit render-side marker LOD
  as a separate slice.
- **Resolution evidence:** undilated headless probe over the example and
  tolerance fixtures (24-node demo, 12-node team start, hub/256 ring, grid/20x20@40):
  average relaxed edge length moved from 4.8 / 3.9 / 10.6 / 9.0 world units
  (default scaling 1) to 35.8 / 29.7 / 73.8 / 33.3 (scaling 432) — every family
  at or beyond ~2.5× the rendered diameter except capacity-limited hub leaves;
  settle iterations stayed far inside the contract budget (156 / 109 / 128 /
  471 against < 1500); `cargo test -p gpui-graph --lib` 217 passed including
  `settles_within_iteration_budget` and `slow_down_scales_per_iteration_motion`.
- **Follow-up (2026-08-23, same day):** the canonical `node_radius` default was
  halved to 3 (diameter 6 world units); by the same pair-equilibrium derivation
  the FA2 scaling default moved 432 → 54 (`d* = (54·2²)^(1/3)` = 6). Fixture
  ratios were preserved (demo/24 ratio 3.01 / team 2.50 / hub 6.45 / grid 3.47;
  settle iterations 204 / 66 / 195 / 528), confirming the calibration is
  scale-free. DESIGN.md §15.1 records the current numbers.
- **Severity:** P2 visual quality — demo-scale graphs relax into overlapping node blobs over
  tens of seconds; static and freshly-opened views are unaffected.
- **Owner:** `crates/gpui-graph/src/layout/force_atlas2.rs` force balance, against the
  world-sized node contract (`3a52ee395`, DESIGN.md §26.2) and the default placement contract
  (DESIGN.md §13).
- **Observed behavior:** Headless probe mirroring the `force_atlas2` / `interactive` examples
  (24-node random hub graph, 12-node start graph, FA2 defaults, one iteration per frame): from
  the ±200-unit spread start, the average edge length falls from ~205–233 world units at frame
  0 to **4.7–6.5 by settle** (frame ~1400 undilated pace) — i.e. below half the rendered node
  diameter (`2 * node_radius` = 12 world units), with a minimum pairwise node distance of
  ~2.3 units. Before world-sized nodes this contraction was invisible (nodes drew at a fixed
  6 px); now the settled state renders as heavily overlapping discs.
- **Mechanism:** linear attraction grows with distance while repulsion falls off as
  `1/d²`, so the pairwise equilibrium sits near `scaling^(1/3)` times small mass factors —
  well inside one node diameter at the default `scaling = 1`. A universal hard separation
  floor was prototyped and rejected: dense topologies make the floor unsatisfiable (hub/256
  leaves around their center equilibrate at ring spacing ~1.2 units; enforcing ≥12 units per
  pair puts the constraint in permanent conflict with the force law, so the layout never
  settles — `layout::force_atlas2::tests::settles_within_iteration_budget` fails at budget),
  and raising attraction-side softening or `scaling` alone either misses the floor or breaks
  convergence contracts elsewhere.
- **Impact:** relaxed layouts eventually render overlapping markers in every app using FA2
  defaults with the canonical style; hit-testing ambiguity follows visually.
- **Next decision:** pick one contract before coding: (a) a size-aware FA2 parameter with a
  capacity-aware relaxation that tolerates density where the floor cannot hold (Gephi-style
  no-overlap is approximate for exactly this reason); (b) render-side marker LOD that shrinks
  or fades markers when local density exceeds what world-sized discs allow; or (c) document
  per-app tuning of `with_scaling`/`node_radius` pairs and accept blobbing for dense hubs.
  Record the decision in DESIGN.md §11/§26.2 and close this entry in the fixing commit.

### GAP-2026-08-22-001 — Migration-driven directed edge builds reject at graph-index seeding: wire vs catalog label identity divergence

- **Status:** Closed (2026-08-23; identity rule decided and implemented per the Next decision
  below — plan 0282). Owning evidence: graph-index unit contract green
  (`cargo test -p gleaph-graph-index --lib` 153 passed / 0 failed, including
  `directed_wire_fact_seeds_under_catalog_target_and_catalog_sieve_finds_it`,
  `any_direction_target_accepts_both_bucket_packings_of_its_catalog_label`,
  `facts_from_a_different_catalog_label_are_rejected_and_store_nothing`, and
  `dml_build_subjects_carry_wire_labels_against_catalog_targets`); cross-canister closing proof
  green (`cargo test -p gleaph-pocket-ic-tests --test adr0059_index_build_lifecycle`
  **5 passed / 0 failed** with both previously-ignored inline scenarios un-ignored and passing:
  `edge_inline_create_index_migration_converges_active_with_complete_postings` converges a
  directed ROAD + undirected LINK migration over pre-existing two-shard inline values with
  posting-level shard/canonical-owner assertions, and
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges` preserves watermarks across a
  same-wasm mid-build upgrade of all five federation canisters); regression guards green
  (`router_gql_query` 24 passed / 0 failed including every standalone edge lifecycle target).
- **Severity:** P0 index correctness — blocks the edge half of ADR 0059's lifecycle and therefore
  the GAP-2026-07-29-001 E2E closure
- **Owner:** The Router→Graph→graph-index edge build identity contract: Router registration
  (`crates/router/src/index_catalog.rs::resolve_index_definition`,
  `facade/store/schema_migration/index.rs::step_request`), Graph export fact emission
  (`crates/graph/src/index/canonical_export.rs::export_edge_sidecar_page` /
  `export_edge_inline_page`), and graph-index identity enforcement
  (`crates/graph-index/src/facade/store/build_state.rs::prepare_fact_posting`)
- **Observed behavior:** Both new PocketIC scenarios in
  `crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`
  (`edge_inline_create_index_migration_converges_active_with_complete_postings`,
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges`, currently `#[ignore]`d) drive a
  combined `CREATE INDEX ... FOR ()-[e:ROAD]-() ON (e.distance)` + undirected LINK migration
  through `apply_schema_migration` over a two-shard federation with pre-existing inline edge
  values. The first (directed ROAD) sub-build registers and reaches Building, then terminates with
  `SchemaMigrationApplyStatus::Failed(TargetRejected)`; in the upgrade variant
  `IndexBuildProgress.seeded_items` stays 0 until the round budget exhausts. Vertex builds are
  unaffected (their facts carry no label).
- **Mechanism (static chain, each link read directly on main `306f469c1`):**
  1. The Router registers the build target with the untagged catalog label id:
     `resolve_index_definition` uses `lookup_edge_label_id(...).raw()` and
     `IndexDefRecord.label_id` persists it; `step_request` copies it into
     `IndexBuildTarget::Edge { label_id }`.
  2. Graph storage keys edges by tagged wire labels (`BUCKET_LABEL_DIRECTED_BIT = 0x8000`;
     directed bucket key = catalog id | 0x8000, undirected = catalog id). Both edge export pages
     emit facts whose `label_id` is that storage value (`key.label_id()` / `edge.label_id()`).
  3. graph-index requires exact equality between fact label and registered target label
     (`target_label == label_id`) and returns `InvalidIndexBuildTarget` otherwise.
  4. `InvalidIndexBuildTarget` is not retryable, so the driver classifies it as
     `MigrationFailureCode::TargetRejected`.
  For a DIRECTED edge, wire (`id | 0x8000`) never equals the registered catalog id, so every seed
  page rejects.
- **Expected or needed behavior:** One owner must define the posting label identity end-to-end so
  migration-seeded postings, DML-maintained postings, and read-side binding agree for both
  directedness buckets. Note the same divergence family exists on the Active DML/read path today:
  DML maintenance inserts postings under wire labels
  (`crates/graph/src/index/edge_pending.rs::push_edge_index_op` receives the canonical wire label)
  while expand lookups sieve with resolved catalog ids
  (`plan/query/executor/expand/candidates.rs`), so directed-edge equality probes through
  graph-index return zero hits and silently degrade to scan fallbacks
  (`expand_candidates_via_equality_index` treats empty postings as "index did not own the lookup").
  Single-shard lifecycle tests pass because of that fallback, masking the divergence.
- **Evidence:** Scenario run 2026-08-22: inventory precondition `router_gql_query` 24 passed /
  0 failed; `adr0059_index_build_lifecycle` 3 passed / 2 failed with
  `Failed(TargetRejected)` and a stuck-at-zero `seeded_items`. Unit coverage gap: graph-index's
  build-state tests use one arbitrary label id (9) for both target and facts, so the divergence
  has no unit-level signal.
- **Impact:** No index over a directed edge property can be created through the migration
  lifecycle (terminal failure); GAP-2026-07-29-001 cannot close. Undirected-only registrations
  happen to satisfy the numeric identity check but then bind read handles from catalog-id
  postings, so fixing only the register side without owning the identity decision would leave
  read binding broken for one bucket packing.
- **Next decision:** Decided (2026-08-22, plan 0282 survey): **edge postings are stored and
  transported in wire-tagged space (`catalog id | directed MSB` directed, bare catalog id
  undirected); registrations, memberships, and lookup sieves speak catalog ids.** Rationale:
  every physical producer already emits wire labels (DML dispatch from canonical handles,
  repair journal, operator backfill export, migration seed facts) and read binding resolves
  `EdgeHandle`s from posting labels against LARA bucket keys, so wire storage needs zero
  producer churn and keeps handle binding unambiguous; the catalog-space alternative would
  force a translation onto every Graph producer and make handle binding direction-ambiguous.
  The single translation owner is `gleaph_graph_kernel::entry::label` — `EdgeLabelId`
  (catalog) ↔ `TaggedEdgeLabelId` (wire) via `pack()` / `label_index()`; no ad-hoc `0x8000`
  arithmetic outside it. Conforming seams: (S1) graph-index `prepare_fact_posting` accepts a
  fact when its wire label's catalog index equals the registered catalog label and its bucket
  is covered by the registration direction, storing under the wire tag (`EdgePostingKey`
  layout and Candid types unchanged); (S2) `ensure_subject_matches_target` applies the same
  rule to DML build subjects; (S3) graph-index `lookup_edge_{equal,range}_page` treat
  request labels as catalog ids and scan both packings; (S4) Graph's
  `lookup_edge_{equal,range}_local` resolve memberships by direct catalog equality instead of
  feeding a catalog id into the wire-expecting registration matcher. Router registration,
  export scopes/walks, planner stats, and all Graph producers stay unchanged.

### GAP-2026-08-21-002 — Graph registration completion held bootstrap intent locks across a deployment's graph bootstraps

- **Status:** Resolved 2026-08-22 by `3ef8fe9d53f503b9663e1e5fe324741a16005559`
- **Severity:** P2 provisioning lifecycle
- **Owner:** Router Graph provisioning convergence and Provision's versionless `complete_graph_registration` endpoint
- **Historical observed behavior (before the 2026-08-22 working-tree fix):** After a successful `CREATE GRAPH` bootstrap, the Router-side request record stayed `AwaitingAck` and its `(deployment_id, GraphShard(0))` intent lock stayed held because no Provision→Router ack arrived. A second `CREATE GRAPH` by the same caller principal failed with `Conflict("provisioning intent already locked")` indefinitely (`adr0070_create_graph_provisioning.rs` first exercised this as an infinite retry).
- **Expected or needed behavior:** Once Router has reconciled the exact Graph topology, its versionless completion call must mark the Provision job `Completed` and release only that request's Map 2/3 and Map 47 rows, so sequential bootstraps converge.
- **Evidence:** Fixing commit `3ef8fe9d53f503b9663e1e5fe324741a16005559`. Exact Router owner
  tests cover retry-before-early-return, partial registration, lost-response replay, byte-exact reopen,
  and owned Map 47 release. The Provision owner lifecycle test drives the production accept and
  completion handlers across an actual Map 1/2/3 reopen, preserves exact created resources, effect
  count, and immutable-envelope digest on admission replay, and proves completed replay leaves a later
  request's Map 2/3 rows unchanged. On 2026-08-22 UTC, both focused PocketIC runtime targets passed:
  `create_graph_provisions_shard_and_sets_home_graph` proved same-caller consecutive graphs, and
  `router_graph_bootstrap_registration_ack_crosses_real_candid_boundary` proved the real
  Router→Provision Candid completion boundary (1 passed, 0 failed for each target).
- **Impact:** The exact `GraphShard(0)` path no longer has a one-graph-per-deployment restriction.
  Property- and Vector-index completion remain deferred to their owner-specific slices.

### GAP-2026-08-21-001 — Anchored multi-DML roll-forward saga fails to converge on both shards

- **Status:** Resolved 2026-08-22 (fix implemented on top of `7a75e9fd6` and validated there; commit pending at time of update)
- **Severity:** P1 DML correctness signal
- **Owner:** Graph vertex-index membership resolution (`crates/graph/src/index/catalog_context.rs::vertex_index_memberships_for_labels`), shared by single DML, bulk insert, and backfill; E2E fixture catalog guards.
- **Observed behavior:** In
  `crates/pocket-ic-tests/tests/router_gql_query.rs`, three consolidated contracts failed:
  `router_runs_anchored_multi_dml_bundle_across_shards_as_roll_forward_saga`,
  `router_recovers_anchored_multi_dml_roll_forward_saga_via_idempotent_retry`,
  `router_recovers_non_terminal_federated_saga_via_idempotent_retry`. The two-shard bundle reports
  `Completed`, but the post-commit read `MATCH (n {age: 6}) RETURN n` returned 0 rows instead of the
  expected 2, so the SET did not survive on either shard despite the completed phase.
- **Mechanism (probe-verified on `7a75e9fd6`):** Nothing removed the old posting. Fixture inserts
  posted through `e2e_legacy_unlabeled_vertex_catalog_guard`, which rewrote every vertex
  membership's `label_id` to 0 so unlabeled vertices matched the label-scoped index; bundle-time
  SET resolved memberships against the true Router catalog, where the same unlabeled vertex
  (`labels == []`) matched nothing, so neither remove-old nor add-new was ever enqueued
  (`pending_min == None` because the queue was never populated). Post-bundle reads still seeded
  from the stale posting (`lookup_equal` hits=2) but the shard-side residual equality filter
  dropped the rows because canonical age had changed. The labeled variant failed admission for a
  second reason: its fixture posted into a fabricated physical namespace (101) instead of the
  Router-allocated namespace (1), so anchor lookups saw no hits.
- **Expected or needed behavior:** A `Completed` roll-forward saga must leave both shards'
  anchor vertices updated; the read-back equality anchor must observe both.
- **Resolution:** `vertex_index_memberships_for_labels` now owns the maintenance rule in one
  place: a vertex with no labels maintains every namespace indexing the property (legacy-unlabeled
  contract, mirroring the Router's label-less superset anchor reads); labeled vertices maintain
  wildcard plus exact-label memberships as before; a missing vertex row dispatches nothing. The
  fixture-only wildcard rewrite and fabricated namespaces were deleted — fixtures fetch the real
  Router catalog via `e2e_router_catalog_guard`. Regression origin diff-triaged to `04575f280`
  (strict label-scoped DML membership resolution), not to the five commits named in plan 0269
  Step 2; no bisect was needed.
- **Owning regression tests:** graph-unit `unlabeled_legacy_set_reposts_remove_and_insert_into_label_scoped_membership`,
  `labeled_set_reposts_only_into_its_own_label_membership`,
  `remove_repost_removes_exactly_the_old_value_posting`,
  `multi_label_vertex_sharing_one_namespace_pushes_no_duplicate_ops`,
  `missing_vertex_row_dispatches_no_postings` (`catalog_context.rs` tests); PocketIC
  `router_labeled_insert_set_repost_converges_across_shards` plus the restored saga trio in
  `router_gql_query.rs` (20 passed / 0 failed at the fix).
- **Evidence:** Probe instrumentation over a pinned worktree of `7a75e9fd6` (no working-tree
  changes): fixture-phase `resolved=[(1,1)]` + `Insert` posting vs bundle-phase `resolved=[]`
  twice per shard; zero index-canister vertex-posting mutations during the bundle; post-bundle
  seed hits=2 sieved to 0 rows by the residual filter; labeled variant pre-bundle eq(5)=0 with
  `ix batch phys=101`.

### GAP-2026-08-20-001 — AtLeast graph-index barrier ignores pending first-delivery outbox work

- **Status:** Resolved (commit pending; fix is in this patch)
- **Severity:** P0 read consistency
- **Owner:** Router ReadMode::AtLeast barrier and the Graph-owned durable derived-index work boundary
- **Observed behavior (before fix):** GraphStore::index_pending_min_mutation_id read only the
  failure-only RepairJournal, while DerivedIndexOutbox was a separate durable FIFO whose entries
  retain their originating mutation_id. Router's AtLeast barrier therefore could consult only the
  repair owner.
- **Expected or needed behavior:** An index-backed AtLeast(M) read must fail closed until every
  derived posting at or below M has reached its target, including first-delivery and failed-flush
  work.
- **Resolution:** Graph MemoryId 52 now owns one fixed key for every qualifying source row in
  DerivedIndexOutbox and RepairJournal. GraphStore prevalidates and synchronously co-updates the
  source row and its exact floor key; zero-id and `IndexBuildDml` rows are excluded, quarantine
  preserves its key, and acknowledgements remove only exact applied rows. Router's existing barrier
  and wire shape are unchanged.
- **Evidence:** `fixed_keys_order_by_mutation_owner_and_sequence`,
  `fresh_and_reopen_preserve_exact_floor`,
  `co_updates_outbox_and_repair_without_zero_or_build_rows`,
  `quarantine_and_partial_ack_preserve_exact_multiplicity`,
  `prevalidation_failure_leaves_owners_and_floor_unchanged`, and PocketIC
  `first_delivery_outbox_survives_graph_upgrade_and_atleast_fails_closed_until_drain`.
  ADR 0029 §5 remains the AtLeast projection-barrier authority.
- **Validation status:** The five exact Graph owner tests, layout/stats tests, focused floor and
  reopen canbenches, Graph check/clippy, and stopped-index PocketIC lifecycle pass. Final unfiltered
  Graph benchmark persistence and final plan/scope gates are recorded in Plan 0263.
- **Impact:** A token can be treated as graph-index satisfied while its first delivery remains
  durable but unapplied, allowing an index-backed read to miss canonical state.
- **Next decision:** Keep the separate Vector Index acknowledgement/watermark/tombstone-retention contract and
  index-build generation visibility contract in their later slices; do not infer either from this
  ordinary Graph-owned floor.

### GAP-2026-08-20-002 — Router direct vector ingestion durable intent ownership

- **Status:** Resolved (2026-08-22; catalog-driven markerless frontier implementation and focused
  runtime/benchmark gates pass; the persisted Router artifact has all 41 terminal benchmark entries,
  including `markerless_frontier_catalog` total 52,080,138 and scope 52,079,143 instructions;
  targeted affected-crate format passes while the workspace-wide format check is blocked by unrelated
  dirty paths and never passed; finite-time liveness, global subject-map growth, and finite-time GC
  remain explicitly deferred)
- **Severity:** P0 durable derived-state contract
- **Owner:** Router direct vector-ingestion API and MemoryId 53 lifecycle
- **Contract:** Every allocated direct-ingestion mutation ID is synchronously co-written with one
  exact durable intent before the first Graph await. `Pending` is valid only while that intent is
  `AwaitingGraph`, `AwaitingVector`, or `AwaitingFrontier` in the sole MemoryId 53 lifecycle. The
  Router row durably owns pending/retry payload bytes until exact marker retirement; Vector owns the
  indexed embedding bytes after delivery.
- **Implementation:** `admit_awaiting_graph` validates the full batch, live shard, exact Graph and
  Vector targets, immutable definition, capacity, and encoded size before changing state. It then
  co-writes the final `ROUTER_MUTATION_COUNTER` value and every canonical `AwaitingGraph` row with no
  intervening await. Exact Graph acceptance changes only the matching row to `AwaitingVector`;
  exact logical rejection changes only that row to `AwaitingFrontier`; transport/decode failure and
  response loss retain the current phase. Vector typed-prefix acknowledgement changes only applied
  `AwaitingVector` rows to `AwaitingFrontier`. The canonical shard catalog enumerates only fully
  attached, non-anonymous `(Vector target, shard)` lanes. For the selected lane, Router derives the
  frontier from the oldest unresolved exact-lane intent minus one, or the durable allocation ceiling
  when none remains, and may publish an empty marker snapshot. Unresolved direct intents in another
  lane do not cross-block. The recovery cursor advances before await and attempts one lane per tick,
  so a failed lane yields later lanes a turn before retry after lap rotation. The Router-only Vector
  endpoint rechecks Router ownership and exact shard attachment, applies MemoryId 15 with `max`, and
  runs one bounded GC step in the same no-await update; its cutoff remains
  `min(graph_watermark, router_watermark)`. Recovery uses persisted targets for direct-ingestion rows
  and catalog discovery only for markerless lane enumeration.
  MemoryId 2 now stores the public shard projection and a Router-private monotonic Vector attach
  epoch in one canonical row. Unregister advances the epoch while clearing both readiness bits
  before the first detach await and returns the exact retained Vector target plus epoch as its
  continuation claim. Router revalidates that claim and both cleared bits before each outbound
  detach and atomically before final removal. Successful existing-row index re-registration
  advances the epoch once while clearing the old target; an already completed duplicate is a no-op.
  A new explicit same-target attach advances the epoch again, so every pre-unregister finalizer and
  unregister continuation exact-conflicts and only the new claim may publish readiness or retain
  the row.
- **Evidence:** `crates/router/src/facade/stable/vector_ingest_outbox.rs::{admit_awaiting_graph,
  observe_graph_accept,observe_graph_reject,apply_outcome,run_recovery_pass}`,
  `crates/router/src/facade/stable/graph_catalog.rs::scan_attached_vector_lane`, and
  `crates/router/src/api/control.rs::ingest_vertex_embeddings`. Owner-local tests cover admission
  atomicity, phase persistence, exact compare-before-transition, stale callbacks, reopen, Vector
  prefix retention, empty/non-empty frontier snapshots, catalog enumeration, one-lane rotation,
  malformed outcomes, and recovery scheduling. The focused PocketIC test
  `graph_only_markerless_lane_advances_frontier_on_autonomous_timer` passes exactly one test with
  seven filtered tests, one `PocketIc`, one federation bootstrap, and four canister installs (Router,
  Property Index, Graph, Vector); it covers ordinary timer dispatch, applied response loss, upgrade
  rediscovery, exact retry, and physical collection under the Vector
  `min(graph_watermark, router_watermark)` cutoff. The persisted Router
  `crates/router/canbench_results.yml` artifact has all 41 terminal benchmark entries;
  `markerless_frontier_catalog` records exactly 52,080,138 total instructions and 52,079,143
  instructions in its benchmark scope. The dated Plan 0277 focused/persisted Vector
  `bench_router_frontier_gc_budget` baseline records 2,110,300,092 total and 2,110,299,087 scoped
  instructions; the later live artifact currently records 2,084,327,695 total and 2,084,326,690
  scoped instructions from unrelated work and is not this slice's evidence. Nine exact Router
  owner unit tests pass:
  `stale_vector_attach_finalizer_after_unregister_reregister_requires_new_attach`,
  `same_target_vector_reattach_claim_fences_delayed_pre_unregister_finalizer`,
  `unregister_start_response_loss_then_register_requires_explicit_vector_reattach`,
  `second_graph_same_vector_target_and_shard_cannot_duplicate_lane_ownership`,
  `markerless_failure_rotates_to_next_lane_and_wraps_without_starvation`,
  `vector_attach_rejects_cross_page_duplicate_before_claiming_candidate`,
  `unregister_rejects_exact_pending_vector_work_without_lifecycle_mutation`,
  `unregister_shard_with_vector_target_removes_vector_readiness_row`, and
  `delayed_unregister_final_commit_cannot_remove_new_same_target_ownership_after_vector_detach`.
  The separate regression
  `delayed_unregister_cannot_detach_or_remove_new_same_target_ownership` also passes and protects
  the post-await unregister claim fence. Router check passes. Router all-target/all-feature clippy
  is blocked only by unrelated unused imports and one needless borrow
  in `crates/router/src/prepared.rs`; production-library strict clippy passes with no markerless
  diagnostic. The targeted affected-crate format check passes
  while the workspace-wide format check is blocked by unrelated dirty paths and never passed. The
  Quint refinement passes 15/15 deterministic tests and both 5,000-sample depth-35 safe-policy runs
  with all requested witnesses; `quint verify` remains unrun and non-required. This bounded model
  and the focused runtime evidence are not production proof of finite-time liveness or global
  subject-map growth.
- **Impact:** Restart or response loss cannot leave an allocated direct-ingestion stamp without a
  durable owner. Fully attached catalog lanes, including Graph-only lanes with no MemoryId 53 row,
  can now advance the Router frontier and use the existing
  `min(graph_watermark, router_watermark)` cutoff. The public operation remains at-least-once
  retry/convergence with no finite-time or client-level exactly-once guarantee, and no global
  subject-map growth or finite-time GC bound is claimed.
- **Next decision:** No remaining prerequisite exists for this bounded durable-ownership and
  markerless eligibility slice. Keep finite-time liveness, global subject-map growth, and finite-time
  GC as separate future evidence; do not infer those claims from the bounded Quint model or the
  one-lane runtime gate.

### GAP-2026-08-20-003 — CanonicalPending retry does not reconcile a completed Graph receipt

- **Status:** Resolved in `d700331c33a8cdae7524e76bcb4e9ddcd5cdb600`
- **Severity:** P1 Router exact-replay recovery
- **Owner:** Router ordered atomic_insert lifecycle and exact Graph journal reconciliation
- **Observed behavior (before fix):** ADR 0049 requires a same-key retry after a Graph success /
  Router callback loss to query the exact Graph journal and either record its completed receipt or
  resend the exact stored request. Ordered edge, vertex, and mixed recovery left CanonicalPending
  with an explicit-retry diagnostic, and an existing same-key request returned its Router record
  without reconciliation.
- **Expected or needed behavior:** If the exact Graph journal contains a completed receipt, retry
  must advance the Router record without canonical redispatch. Only an explicit exact retry may
  redispatch an unambiguously absent stored target; background recovery must remain query-only and
  fail closed.
- **Resolution:** Existing ordered CanonicalPending admission now reaches Router's private
  trigger-aware reconciliation boundary before fresh routing or catalog resolution. It reads the
  persisted Graph target and accepts only exact active, completed journal evidence to record the
  matching receipt without an ordered execution. Explicit same-key retry may redispatch only after
  an unambiguous Absent result and only with that immutable stored request. Background recovery is
  query-only: it may adopt exact completed evidence, while Absent, invalid, retired,
  not-applicable, non-completed, and identity/version/fingerprint/count/allocation mismatches keep
  CanonicalPending with only the bounded diagnostic change and no ordered execution.
- **Evidence:** Router tests
  canonical_pending_reconciliation_uses_stored_target_before_fresh_resolution,
  canonical_pending_reconciliation_adopts_exact_completed_receipts_without_dispatch,
  canonical_pending_reconciliation_rejects_nonexact_evidence_without_dispatch, and
  canonical_pending_reconciliation_handles_absent_by_trigger; PocketIC tests
  atomic_insert_canonical_pending_reconciliation_same_key_retry_does_not_redispatch_completed_journal
  and
  atomic_insert_canonical_pending_reconciliation_timer_after_router_upgrade_does_not_redispatch_completed_journal.
  The final focused Router filter reported 4 passed / 792 filtered. The focused PocketIC filter
  reported 2 passed / 13 filtered after the test-only target_shard expectation correction. Router
  library clippy, Graph pocket-ic-e2e clippy, and the PocketIC target clippy passed.
- **Supplemental runtime evidence:** After the latest Router reservation fix, the full
  adr0057_atomic_insert target ran once and reported 14 passed / 1 failed. Both reconciliation
  tests passed. This target is not counted as a pass: the unrelated
  atomic_insert_rejects_missing_catalog_name_before_reservation test failed because PocketIC HTTP
  adapter startup timed out and reqwest reported IncompleteMessage.
- **Close-gate status:** Global `cargo fmt --all -- --check` passed. The final Plan 0266 validator
  passed. The final exact 7-path scope review passed with the staged-empty/unrelated manifest
  matched. Both scoped Plan tests passed. The full `adr0057_atomic_insert` target remains
  incomplete at 14 passed / 1 failed because the unrelated
  `atomic_insert_rejects_missing_catalog_name_before_reservation` test hit a PocketIC HTTP
  adapter startup timeout (`IncompleteMessage`). No benchmark was run because this slice does not
  change a performance-sensitive algorithm. The recovery fix was committed as
  `d700331c33a8cdae7524e76bcb4e9ddcd5cdb600`.
- **Impact (before fix):** A committed canonical effect could remain non-terminal despite the
  exact durable Graph receipt needed to reconcile it.
- **Next decision:** None for this recovery contract. Plan 0260 remains a later, non-blocking
  formalization slice.

### GAP-2026-08-20-004 — Rust application client SDK and shared Router wire contract

- **Status:** Resolved
- **Severity:** P2 product gap; generated-code quality gap for the Rust client profile
- **Owner:** `gleaph-codegen` rust client profile, `gleaph-cdk`, and the SDK boundary
- **Observed behavior:** The Rust application-client codegen profile (`generate_rust`) emitted a
  provisional `PreparedExecutor` / `PreparedQueries` facade whose transport and response decoding
  were a runtime scaffold, and it did not mirror the mature `PreparedExt` profile used by
  `gleaph-cdk`. There was no supported Rust application client; the Router data-plane wire contract
  lived only in `gleaph-cdk::types`, which depends on `ic-cdk` and is not a valid application-client
  dependency.
- **Resolution (ADR 0069, 2026-08-20):** introduced `gleaph-router-wire` (`crates/router-wire`) as a
  neutral owner of the Router data-plane wire contract (`GqlQueryResult`, `ReadMode`,
  `MutationToken`, `RouterError`, bulk-load family, row-decode helpers); `gleaph-cdk` re-exports it.
  Introduced `gleaph-sdk` (`sdk/client/rust`), an `ic-agent` application client mirroring the
  `GleaphClient<Prepared>` surface with a `GleaphTransport` trait, `IcAgentTransport`, and caller
  identity injection through the agent identity. Unified the Rust codegen profiles onto a shared
  renderer (`crates/codegen/src/rust/shared.rs`) parameterized by runtime path; the client profile
  now emits the same `PreparedExt` boundary as the canister profile, removing `PreparedExecutor` /
  `PreparedQueries` and the provisional client envelope/structs.
- **Evidence:** `crates/router-wire`, `sdk/client/rust`, `crates/codegen/src/rust/shared.rs`;
  updated fixture at `crates/codegen/fixtures/rust-client-basic/src/lib.rs`; codegen tests in
  `crates/codegen/src/lib.rs`.
- **Impact:** Resolved: application clients get a typed, canister-parity client with identity
  injection, and both Rust profiles share one generated `PreparedExt` shape and one wire contract.
- **Next decision:** None — ADR 0069 accepted; the generated Rust client output changed from the
  `PreparedExecutor` scaffold to `PreparedExt`, so any scaffold consumer must regenerate.

### GAP-2026-08-20-006 — Router shard identity has no incarnation or lifecycle fence

- **Status:** Resolved (2026-08-22)
- **Severity:** P0 identity safety
- **Owner:** Router graph catalog lifecycle, ShardId allocation, and Graph/Router wire identity
- **Resolution:** `ROUTER_GRAPH_RUNTIME_CONFIG.next_shard_id` is the canonical per-graph high-water.
  Registration accepts only that never-issued id and advances the high-water; unregister and
  failed-attach rollback never rewind it. Exhaustion fails before any registry mutation.
- **Evidence:** `graph_runtime_config_reopen_preserves_shard_high_water`,
  `unregister_shard_never_reuses_the_retired_graph_local_id`,
  `exhausted_graph_local_shard_allocator_rejects_without_registry_mutation`, and
  `registry_invariants_reject_active_shard_at_allocator_high_water`.
- **Impact:** Delayed work and public identifiers from a retired shard cannot alias a later shard
  within the same graph, without widening `GlobalVertexId` or cross-canister protocols.
- **Next decision:** None for shard identity. Frontier publication remains a separate Vector GC
  protocol decision.

### GAP-2026-08-20-005 — Property DROP INDEX has no durable per-PhysicalIndexId retirement lifecycle

- **Status:** Resolved (2026-08-22; fix is in this patch)
- **Severity:** P1 derived-index cleanup
- **Owner:** Router index catalog and graph-index posting purge lifecycle
- **Observed behavior:** Router removes the catalog row before remote purge and keeps purge progress
  only in the active call. A retry after response loss cannot recover the removed catalog identity.
  Multiple physical namespaces can exist for one logical property, while the remaining-reference
  decision can skip or target only one namespace: dropping one of several indexes on a shared
  property skipped the purge entirely, orphaning the dropped namespace immediately, and the final
  drop purged only its own namespace.
- **Expected or needed behavior:** Every unreferenced PhysicalIndexId must have a durable,
  resumable purge identity and cursor until graph-index confirms cleanup; a shared logical property
  must not leak a distinct physical namespace after its final reference is removed.
- **Resolution:** `DROP INDEX` now co-writes the catalog removal with a durable Router-owned
  retirement record keyed by `PhysicalIndexId` (`ROUTER_INDEX_RETIRED`, MemoryId 54) that freezes
  the drain target set resolved before the destructive mutation and persists one resume cursor per
  pending target. Posting scopes are disjoint per physical namespace and unreachable once the
  catalog row dies, so every dropped definition retires unconditionally — the wrong
  remaining-reference gate is deleted. The inline fast path drains bounded rounds inside the DDL;
  holds defer to a new recovery-timer lane (`index_retirement.rs`, Driver 4) advancing one bounded
  step per pending target per tick; records delete exactly when the last frozen target confirms
  done. `DROP INDEX` returns success after the co-write and never fails on purge transport errors.
- **Owning regression tests:** PocketIC
  `dropping_one_of_two_indexes_on_shared_property_does_not_leak_its_namespace` (per-namespace leak,
  plus sibling-survival guard) alongside the restored `drop_index_purges_postings_from_graph_index`
  and timer-compaction contracts in `adr0023_index_store_consistency.rs` (6 passed); router unit
  tests in `router/src/index_retirement.rs`: response-loss hold-and-converge, cross-pass cursor
  persistence, per-target partial completion, empty-pending retirement, scan pagination; memory
  layout inventory updated to 55 regions.
- **Evidence:** Failure modes F1–F5 traced through `router::drop_index` → `drop_named_index`
  (catalog row removed first) → call-local resume loop over `graph_index_lookup_targets`; probe
  verified postings are keyed per `(physical_index_id, property_id)` so per-namespace retirement is
  complete by construction. The Quint model named in the next decision is unnecessary: the
  retirement record is a single-owner monotonic lifecycle (enqueue → per-target drain → delete)
  with no concurrency beyond the idempotent bounded steps, which the unit regressions cover.

### GAP-2026-08-11-004 — Current V1 metadata boundary leaves too little extension space

- **Status:** Resolved (obsolete)
- **Severity:** P2 / Persisted-layout capacity
- **Owner:** `ic-stable-clustered-hash-map` header and entry-address invariants
- **Observed behavior:** The current V1 metadata fields occupy bytes 0 through 62 and the table
  entry boundary is now defined at byte 128. Fresh layout creation clears the complete metadata
  extension.
- **Expected or needed behavior:** Future persisted metadata must fit within the 128-byte current
  V1 prefix without moving entries again.
- **Evidence:** `header.rs::HEADER_SIZE`, `header.rs::DATA_OFFSET`, and the test
  `current_v1_header_uses_128_byte_data_boundary` establish the boundary and initialization.
- **Impact:** The map has space for additional persisted state while retaining the V1 version
  number and fixed field offsets. The extra prefix is allocated once per map and does not change
  entry stride or mutation algorithms.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-002 — Active-remap overflow can force a full remap drain in one insert

- **Status:** Resolved (obsolete)
- **Severity:** P1 / High bounded-execution risk
- **Owner:** `ic-stable-clustered-hash-map` capacity, incremental-remap, and mutation-atomicity
  invariants
- **Observed behavior:** The header stores logical capacity at offset 29. `size_up()` accepts only a
  settled table, preserves the existing tail reserve while doubling buckets, and contains no
  active-remap drain. Normal and remap relocation preflight the full chain and extend a cleared
  64-slot tail before the first destructive write. `REMAP_BATCH = 64` counts every examined
  position. Public `insert` and `remove` keep the direct path when the final `REMAP_BATCH + 1` slots
  are empty, which bounds all maintenance relocations plus the requested insert without growth.
  Other growth-capable operations write directly through a block-granular undo transaction. It
  snapshots each overwritten block below the operation's initial logical capacity at most once,
  delegates exact growth and writes to the backing memory, and restores those disjoint blocks if the
  operation returns an error. Writes in a newly grown logical tail need no snapshot because restoring
  the original header makes them unreachable.
- **Expected or needed behavior:** No public mutation should perform a table-sized remap fallback.
  Capacity pressure must be handled before relocation writes begin, preserve the current two-mapping
  (`N` / `N-1`) mixed-lookup contract, and leave the key set and header recoverable if stable-memory
  growth fails. A failed public mutation must return the exact maintenance error with the whole
  operation's logical bytes, header, length, capacity, remap boundary, and key set unchanged.
- **Evidence:** `header.rs::CAPACITY_OFFSET` and `map.rs::capacity` establish the persisted source of
  truth. `extend_tail`, `insert_and_relocate`, `remap_position`, and settled-only `size_up` enforce
  grow -> clear -> publish ordering and preserve N/N-1 lookup.
  `multi_boundary_active_remap_insert_oom_is_operation_atomic_and_retry_succeeds` and
  `multi_boundary_active_remap_remove_oom_is_operation_atomic_and_retry_succeeds` prove exact-byte,
  header, key-set, reopen, and successful-retry behavior when a later remap boundary needs growth.
  The preceding persisted unfiltered `canbench` run uses `DefaultMemoryImpl` and measures the
  generic clustered insert at 67.62M instructions on the wasm32 canbench target; the focused
  active-remap fixtures also complete under the target-selected stable-memory backend. A
  post-change non-persist check on 2026-08-11 measured 75.26M for the generic insert, 37.88K for
  the active batch, and 48.43K for active tail extension; the checked-in artifact was not rewritten
  by that check. These values are not directly comparable with the former explicit-`VectorMemory`
  artifact.
- **Impact:** The table-sized active-remap fallback remains removed. A public `insert` or `remove`
  now either commits its bounded maintenance and requested mutation together or returns
  `OutOfMemory`/`CapacityOverflow` without changing logical map state. Stable-memory page count is
  physical allocation and need not shrink if an earlier grow succeeded before a later failure. The
  undo transaction covers returned errors; traps rely on the IC message rollback boundary, and the
  map adds no standalone write-ahead journal for process-crash recovery outside that boundary.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-003 — Settled `size_up` clears a capacity-scale region in one mutation

- **Status:** Resolved (obsolete)
- **Severity:** P1 / High bounded-execution risk
- **Owner:** `ic-stable-clustered-hash-map` settled resize initialization and persisted-capacity
  invariants
- **Observed behavior:** Active remap maintenance is bounded to `REMAP_BATCH = 64` positions and
  production no longer drains a whole active remap. A settled threshold insert now persists a
  target capacity and clear cursor, grows/clears only a bounded prefix, and retains the old N until
  publication. Physical growth is also extended incrementally as the cursor advances. Settled
  inserts now guard lookup/relocation at 4,096 occupied entries; an over-budget request advances
  one persisted resize/remap step and returns `InsertError::RelocationBudgetExceeded` before the
  request mutates the table.
- **Expected or needed behavior:** The public mutation path must have a per-call initialization bound
  independent of table capacity. New-region initialization is resumable and persisted: each
  operation clears only a bounded chunk, reopen resumes from the cursor, the new bucket mapping is
  not published before the entire region is initialized, and every pending mutation advances the
  cursor or completes the resize.
- **Evidence:** The owning paths are `header.rs::ResizeState`, `map.rs::advance_resize_initialization`,
  `map.rs::publish_resize`, and `map.rs::clear_region`. The deterministic tests
  `threshold_resize_clears_a_bounded_prefix_and_reopens`,
  `pending_resize_clear_oom_rolls_back_cursor_and_reopens`,
  `publishing_resize_marker_reopens_and_finishes_metadata_commit`, and
  `clear_new_aborts_pending_resize_and_reopens_settled` cover cursor progress, reopen, OOM rollback,
  metadata recovery, and reset behavior. The updated N=13/N=16/N=20/N=23 threshold fixtures keep
  setup outside the timed closure and each measures 17,635 scoped instructions for the bounded
  64-slot prefix; the exact persisted values are in
  `crates/ic-stable-clustered-hash-map/canbench_results.yml`. The added settled collision-chain
  sweep measures exact relocation of 4, 64, 256, 1,024, and 4,096 occupied slots. The subject-width
  model (13-byte key plus 41-byte value) measures 600,576 / 2,390,784 / 9,551,635 instructions at
  256 / 1,024 / 4,096 slots, respectively, with an empirical fit of approximately `2,331 * C +
  3,837`. The fixture is setup outside the timed closure and verifies every resident after the
  insert; it is a map-algorithm model, not a vector-canister call-path benchmark. The regression
  tests `settled_chain_budget_starts_persisted_resize_and_retry_advances` and
  `settled_chain_budget_recovery_oom_is_atomic` cover persisted progress, reopen, and exact-byte
  rollback at the new boundary. The post-change non-persist threshold check measured 18.10K scoped
  instructions for each N=13/N=16/N=20/N=23 fixture; the persisted artifact remains the preceding
  17.635K baseline until an explicit final persist run. The bounded insert path now reuses the
  cluster-end boundaries collected by its preflight. A new 4,096-entry single-cluster fixture
  measured 950.56K scoped instructions with the plan versus 2.05M in a controlled plan-disabled
  run (approximately 53.6% lower); this exploratory benchmark is not persisted yet.
- **Impact:** The capacity-scale `clear_region` write is no longer performed in one public mutation,
  and the target mapping is not advertised until the cursor reaches the target. The scale benchmark
  still includes target-selected stable-memory growth and transaction overhead. Settled and active
  relocation attempts now have a deterministic 4,096-occupied-entry guard; the returned error
  commits bounded persisted maintenance while the request key/value remains the caller's
  responsibility. Active remap persists a lower scan cursor when a lower range can be processed,
  but a blocked boundary is retained rather than skipped. Stable-memory pages grown before a later
  returned error remain physical backing outside the logical rollback contract.
- **Next decision:** **Obsolete.** The `ic-stable-clustered-hash-map` crate is retired; its last
  consumer (`VECTOR_PARTITION_HEADS`) migrated to `ic-stable-linear-hash-map` (ADR 0067) and the
  crate was removed from the workspace. This gap no longer applies.

### GAP-2026-08-11-001 — Inner resize leaves the pending insert distance stale

- **Status:** Resolved by commit `1efac368d` (crate retired; see ADR 0067)
- **Severity:** P1 / High
- **Owner:** `ic-stable-clustered-hash-map` inner insert resize and pending-entry distance
  invariant
- **Observed behavior:** In the defective inner `insert_and_relocate` resize branch,
  `size_up()` recalculates the pending entry's bucket and insertion position under the grown
  mapping but leaves `entry.distance` from the prior mapping. The public insert returns `Ok`,
  `len` increases, and iteration contains the entry, while `get` and `contains_key` fail both
  immediately and after re-open.
- **Expected or needed behavior:** After an inner resize, the pending entry's stored distance
  must be the checked distance from its recomputed bucket to its recomputed position before
  relocation or the terminal write continues.
- **Evidence:** `crates/ic-stable-clustered-hash-map/src/map.rs::insert_and_relocate` is the
  owning branch. The repair adds
  `entry.distance = checked_distance(position - b)` immediately after the recomputed bucket and
  position. The focused regression target
  `map::tests::inner_resize_recomputes_relocated_entry_distance` covers successful insert,
  `len`/iterator presence, and immediate plus `init`/re-open point lookup. The repair and
  regression passed the focused Rust test, full crate library tests, format/check/clippy gates,
  and independent review. The repair is recorded in commit `1efac368d`.
- **Impact:** A successful public insert can persist an entry that iteration exposes but point
  lookup cannot reach, and the inconsistency survives re-open.
- **Next decision:** None for this defect; retain the focused regression with the owning map
  implementation.

### GAP-2026-08-10-001 — Future row-copy migration must carry the I8 per-row scale

- **Status:** Open
- **Severity:** P2 (latent; no code path today)
- **Owner:** vector canister slab page store / migration primitive
- **Observed behavior:** With `VectorEncoding::I8` implemented (Slice 0242), stored rows are an i8
  payload plus a per-row f32 scale in row-meta aux. The design's row-copy migration primitive
  (`export_vector_rows` / `import_vector_rows`) is **not implemented**; grep confirms no such code
  exists yet.
- **Expected or needed behavior:** A future export/import that copies rows across canisters (shard
  split, ADR 0031 multi-canister) must carry the per-row aux (scale) alongside the payload, or the
  imported I8 rows would be decoded with a wrong/zero scale.
- **Owner:** vector canister `PAGE_STORE` / `export_vector_rows` future slice.
- **Evidence:** `crates/vector-canister/src/facade/stable/page_store.rs` (`read_row_bytes` returns
  `(vertex_id, bytes, aux)`); `design/index/vector-index.md` §Multi-canister readiness.
- **Impact:** Latent until migration is built; adding it without carrying aux would silently corrupt
  I8 distances on the target canister.
- **Next decision:** When the migration primitive slice is planned, require `export_vector_rows` /
  `import_vector_rows` to round-trip the row aux.

### GAP-2026-08-10-002 — `size_up` under-allocates the canonical entry stride

- **Status:** Resolved by commit `c1dc31db7` (crate retired; see ADR 0067)
- **Severity:** P1 / High
- **Owner:** `ic-stable-clustered-hash-map` entry allocation/layout and `size_up`
- **Observed behavior:** Before the repair, `entry_stride()` used the canonical
  `key_size + value_size + 4` layout, but `size_up` computed its growth target with a stale
  `key_size + value_size + 2` stride.
  At the `n = 13 → 14` capacity of 16,398, the stale `+ 2` target under-requests the
  canonical allocation by `2 * capacity = 2 * 16,398 = 32,796 bytes`.
- **Expected or needed behavior:** Every growth target must cover the canonical allocation
  used by `entry_offset` / `entry_stride`, before clearing or writing the new region.
- **Evidence:** `crates/ic-stable-clustered-hash-map/src/map.rs::size_up` now derives the growth
  target from `self.entry_stride()`. The
  `load_threshold_resize_allocates_the_canonical_entry_stride` regression starts from fresh
  minimal-page backing, deterministically seeds a collision-free `n = 13` table at the normal
  75% load threshold, triggers growth through public `insert`, and asserts that the backing covers
  `DATA_OFFSET + capacity * entry_stride`. The focused test failed before the repair with
  `VectorMemory` `write: out of bounds` and passes after it.
- **Impact:** Before the repair, pre-grown backing could mask the physical out-of-bounds access,
  and collisions could trigger the stale-stride path earlier. Under a conforming `Memory`
  implementation whose out-of-bounds access traps or panics, the failure does not silently corrupt
  the pre-existing old-table bytes; this conclusion does not cover non-conforming memory
  implementations. The repaired path allocates from the canonical stride before clearing or
  writing the grown region.
- **Next decision:** None for the implementation. Record the primary-owned fixing commit after it
  is created.

### GAP-2026-08-04-002 — Rust canister bindings for record parameters lack a Candid form

- **Status:** Resolved
- **Severity:** P2 generated-code compile gap for `Record` parameters only
- **Owner:** `gleaph-codegen` rust-canister profile and `gleaph-cdk` record parameter type
- **Observed behavior:** The rust-canister profile always derived `CandidType` on generated
  `*Params` structs. Record-typed parameters use `gleaph_cdk::GqlRecord` (= `Vec<(String,
GqlValue)>`), whose `GqlValue` element type is candid-free by design in `gleaph-gql-value`,
  so a manifest with a Record parameter emitted bindings that failed to compile with
  `E0277: the trait bound GqlValue: CandidType is not satisfied`.
- **Resolution:** The rust-canister profile now emits `*Params` structs with only
  `#[derive(Clone, Debug)]` — no `CandidType`, no serde. Parameters never cross a candid or
  serde boundary; the generated `into_gql_params()` converts them to ordered logical GQL
  parameters at call time, which also allows raw `f128` / `f256::f256` fields for
  `Float128` / `Float256` parameters. Generated `*Row` structs and `PreparedResponse` derive
  `CandidType` + serde, with exotic row fields bound through the cdk row-binding wrappers
  (`GqlInt256`, `GqlUint256`, `GqlDecimal`, `GqlFloat16`, `Float128`, `Float256` in
  `gleaph-gql-ic-wire`, each with candid + serde impls; `Principal` is candid-native), so a
  canister can return prepared rows directly over its candid interface.
- **Evidence:** `crates/codegen/src/rust/canister.rs` (`canister_param_rust_type` emits
  `gleaph_cdk::GqlRecord`); scratch compile at `/tmp/gleaph-exotic-check` with the
  `exotic_manifest.json` (Int256/Uint256/Decimal/Principal/Float16/Float256 params and rows)
  compiles and round-trips a `PreparedResponse<FindExoticRow>` through `candid::encode_one` /
  `candid::decode_one`.
- **Impact:** Resolved for all parameter and column types; `Record` parameters compile because
  `*Params` no longer requires `CandidType`.
- **Next decision:** None — resolved in the real-type row-binding refactor.

### GAP-2026-07-25-002 — Tombstone-heavy OFFSET scans lack a persistent skip structure

- **Status:** Resolved for the tree regime by [ADR 0094](adr/0094-ltb-block-tombstone-count-level1-offset.md)
  + Plan 0338 (`73ba30e01` field/maintenance, `ec52f28a8` read path, 2026-09-07): the LTB block
  header carries a per-block `u16 tombstone_count` maintained by the single tree-mode remove
  funnel, and `visit_edges_window`'s tree arm resolves only the blocks overlapping the window's
  position range (S1, arithmetic under the Plan 0327 tombstone-inclusive position contract)
  skipping fully-dead in-window blocks via the header count (S2, fail-closed scan
  self-verification). Measured attribution (ADR 0094 implementation-status appendix): the
  dead-prefix paging case reclaims ≈ 99.97% of the tree walk (39.79M → 14,399 instructions,
  S1-dominated); a window spanning 512 fully-dead blocks removes their payload reads at ≈ 29,151
  instructions per skip (S1+S2 = 40.92M vs S0 64.29M, S2-dominated). **Slab/bypass regimes remain
  intentionally status-quo** (research doc §5.5): slab extent is capped at `T_PROMOTE = 4,096`, the
  worst-case re-anchored overshoot is ~109K instructions/query (Plan 0337 slab leg, 1.09× above
  the declared 100K bar — deferred-with-evidence, not opened), the compact-or-promote insert-path
  cycle answers churn, and the remove-side maintenance-admission trigger for Insertion-policy slab
  buckets and bypass rows is recorded as a separate deferred improvement (research doc §5.5).
  Level 2 (a dedicated contiguous per-bucket directory region) stays evidence-gated (§5.2) —
  Level 1 already reclaims ~98% of the measured tree gap.
- **Severity:** P2 traversal performance and stable-layout research gap (closed for tree;
  slab-side residual recorded as deferred-with-evidence)
- **Owner:** `ic-stable-lara` traversal and LARA logical-slot metadata
- **Observed behavior:** `TraversalWindow.offset` can jump directly only for a proven dense bucket.
  Sparse or tombstone-bearing buckets inspect logical rows to count live matches before delivering
  the requested window. No persistent interval summary, live bitmap, or rank/select structure is
  currently part of the LARA layout.
- **Expected or needed behavior:** A future implementation may skip tombstone-only regions and
  resolve the requested live ordinal without inspecting every preceding logical slot, while
  preserving logical-slot identity, exact forward/reverse ordering, overflow semantics, and
  fail-closed corruption handling.
- **Evidence:** [ADR 0050](adr/0050-lara-traverse-read-api.md) § “Known gap: tombstone-aware offset
  acceleration”; `TraversalWindow` and the labeled sparse traversal implementation. The current
  dense fast path is intentionally limited to `live_degree == logical_extent`. Plan 0283 measured
  872,634 instructions at extent 8,192 / 87.5% tombstones / OFFSET 960 / LIMIT 32, versus 38,051
  at the same density and offset 0 and 27,146 for the dense OFFSET control. Fair benchmark-only
  end-to-end probes performed canonical stable-row decoding, liveness checks, slot reconstruction,
  and identical visitor work: fixed-block counts measured 35,620 / 65,776 instructions at 75% /
  87.5%, while triggered two-level counts measured 32,138 / 59,210. The exact temporary patch and
  raw canbench CSV are retained under `design/investigations/artifacts/`. The bounded
  production-owned maintenance drain took 831,216,051 instructions across 1,025 one-work-item
  calls, then restored the query to 27,095 instructions. The sign-corrected optimistic query-only
  crossover is `Q_upper = 831,216,051 / (59,210 - 27,095) = 25,882.49 queries`. This upper bound
  omits candidate build, mutation, update, validation, framing, and repair costs, while compaction
  retains independent storage obligations; a positive crossover alone does not establish
  substitution or workload benefit. The stale predecessor wording was accidentally included in
  unrelated commit `b787ac389`; this hunk records corrected evidence without rewriting that commit.
- **Impact:** Large sparse buckets can spend instructions scanning tombstones for OFFSET/LIMIT.
  Introducing durable metadata now would expand the storage format and add mutation,
  compaction/rebuild, reopen, and benchmark obligations before the design is understood.
- **Next decision:** None for the tree regime — resolved by ADR 0094 + Plan 0338 (S1+S2, measured,
  attribution recorded in the ADR implementation-status appendix). The Plan 0283-era numbers below
  are historical: the extent-8,192 slab fixture predates ADR 0088's `T_PROMOTE` slab cap (legal
  ceiling 4,096 today), and the candidate-C reactive design was rejected by Plan 0336's fixed gate.
  Slab-side residual: status-quo + deferred-with-evidence (research doc §5.5); Level 2 escalation
  evidence-gated. The 2026-08-23 deferred decision text is preserved below as the historical
  record of that slice's scope boundary.

  **Historical record (2026-08-23, Plan 0283 decision at that time):** Retain the exact sparse
  scan and current compaction ownership; do not open an ADR from that slice. Revisit only if
  integrated candidate build/mutation/update/validation costs and a demonstrated workload
  establish a substitution benefit beyond compaction's query and storage obligations. Any
  adoption remains a separate ADR/storage slice.

### GAP-2026-07-25-001 — Paired canonical edge writes expose recoverable post-write errors

- **Status:** Resolved by Plan 0182
- **Severity:** P1 canonical consistency risk
- **Owner:** `ic-stable-lara` bidirectional owner and GraphStore paired mutation boundary
- **Observed behavior:** The owner writes paired halves sequentially. The implementation traps on
  reverse-half and post-write maintenance-admission failures, and dirty-key admission restores its
  bitmap bit when queue append fails. Deterministic failure-injection tests cover directed,
  undirected, and maintenance-queue paths.
- **Expected or needed behavior:** After the first canonical half is written, the supported
  paired-mutation boundary must either complete both halves and all required mutation intent, or
  trap/rollback the whole message segment. A recoverable `Err` must not expose a partially updated
  bidirectional graph to a caller that handles the result without trapping.
- **Evidence:** `crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs`
  (`insert_directed_edge_with_locations`, `insert_undirected_deferred_with_locations`) and
  `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md` § “Shard-local
  preflight and commit discipline”. Graph reverse-adjacency tests intentionally inject and repair
  forward-only and reverse-only states, proving that the state is observable when the paired
  boundary is bypassed.
- **Impact:** A lower-level caller or a handler that returns an ordinary error could leave forward
  and reverse canonical adjacency out of sync, while the counterpart sidecar is already invalidated.
- **Next decision:** Keep exact counterpart validation for repair and published lookup. Any future
  mutation path must preserve the same preflight-then-trap boundary; a new recoverable post-write
  error requires a separate atomicity design review.

### GAP-2026-07-11-005 — `FreeSpanStore` reopen validation has no production-scale cost bound

- **Status:** Resolved (2026-08-22; declared reopen envelope with measured scale probes — up to
  262,144 non-coalescing active spans per store must validate within 1.0B wasm instructions and
  ≤8 MiB transient heap; measured 774.92M instructions at the declared maximum, essentially linear
  at ~2,956 instr/span, so the existing fail-closed sorted-merge validator is accepted unchanged
  and no ADR 0007/0039 amendment is required. Owning benchmarks:
  `bench_lara_free_span_store_reopen_16384/65536/262144` in
  `crates/ic-stable-lara/src/lara/edge/free_span/bench.rs`, thresholds documented in the bench
  header and [storage/lara.md](storage/lara.md) "Reopen integrity". Fragmentation beyond the
  declared level is outside the supported upgrade envelope rather than trapped.)
- **Severity:** P2 operational scalability risk
- **Owner:** `ic-stable-lara` free-span persistence and Graph upgrade preflight
- **Observed behavior:** `FreeSpanStore::init` validates the records/bin ↔ `by_start` bijection by
  collecting all active `(start_slot, id)` pairs, sorting them in heap, and comparing them with the
  ordered index. The algorithm is `O(active log active)` with `O(active)` transient heap. Existing
  canbench coverage measures at most 4,096 active spans.
- **Expected or needed behavior:** before production durable upgrades are claimed, the owner must
  define and measure a maximum supported fragmentation level, reopen instruction ceiling, and
  transient-heap ceiling. Reopen must remain fail-closed; performance work must not weaken the
  bin/index bijection.
- **Evidence:** `crates/ic-stable-lara/src/lara/edge/free_span.rs::validate`;
  `crates/ic-stable-lara/src/lara/edge/free_span/bench.rs::bench_reopen`;
  [storage/lara.md](storage/lara.md) “Reopen integrity”; and ADR 0039 “Upgrade preflight” plus
  “Performance and capacity gates”.
- **Impact:** a highly fragmented long-lived graph could make `post_upgrade` exceed its instruction
  or heap budget even though its stable layout is valid, preventing the new Wasm from serving.
- **Next decision:** first add scale-probing benchmarks beyond 4,096 spans and predeclare acceptance
  limits. If the current validator exceeds them, compare bounded incremental validation, persisted
  validation summaries with generation fencing, and an explicit hard fragmentation cap. Any change
  to the fail-closed validation contract requires an ADR 0007/0039 amendment.
- **Related contracts:** [ADR 0007](adr/0007-stable-memory-layout.md),
  [ADR 0039](adr/0039-production-stable-memory-evolution-and-upgrade-safety.md),
  [storage/lara.md](storage/lara.md)

### GAP-2026-07-11-004 — Non-tail homogeneous bypass insertion rewrites successor origins in `O(V)`

- **Status:** Resolved (2026-08-22; eager promotion — the labeled insert dispatcher promotes a
  non-tail default-bypass row to bucket mode via the existing preflighted
  `promote_bypass_to_bucket_mode` transition before its next same-label insert, so slab-region
  extension and the successor-origin bump only ever run while the row is the tail. Measured scale
  probes `bench_labeled_non_tail_bypass_insert_256/1024/4096`: pre-fix 1.18M / 4.40M / 17.25M
  instructions for 16 inserts (linear, ~263 instr/successor-row), post-fix 343K / 363K / 382K
  (flat; +11% for 16× successors, bounded by the owning PMA leaf). Scan semantics reuse the
  established promotion transition; no persisted row meaning, scan geometry, or PMA ownership
  change, so no dedicated ADR is required. Owning tests:
  `same_label_insert_into_non_tail_bypass_row_promotes_to_bucket_mode` (fails if the promotion
  guard is removed — row would stay default-edge-labeled) and
  `same_label_insert_into_tail_bypass_row_stays_in_bypass_mode`; contract documented in
  [storage/lara.md](storage/lara.md) "Labeled LARA". A `debug_assert` on
  `may_use_homogeneous_bypass` now guards `insert_homogeneous_bypass_edge` directly.)
- **Severity:** P2 performance risk
- **Owner:** `ic-stable-lara` labeled adjacency geometry
- **Observed behavior:** after a homogeneous bypass edge insert,
  `bump_successor_origins_after_bypass_end` scans every later vertex row and rewrites each later
  bypass origin that falls before the new region end. A bypass vertex that ceases to be the tail as
  more vertices are appended can therefore make one later edge insert proportional to the number of
  successor vertices.
- **Expected or needed behavior:** insertion cost must remain bounded by the owning PMA leaf/segment
  or the system must explicitly prohibit or promote non-tail bypass rows before they enter this hot
  path. Scan semantics and the direct vertex-row lookup contract must remain unchanged unless a
  reviewed representation decision replaces them.
- **Evidence:** `crates/ic-stable-lara/src/labeled/graph/bypass.rs::insert_homogeneous_bypass_edge`
  and `bump_successor_origins_after_bypass_end`; existing bypass canbench coverage does not isolate
  repeated inserts into an old bypass vertex across increasing successor counts.
- **Impact:** repeated insertion into an early bypass vertex can grow toward `O(EV)` work and stable
  row writes, creating an instruction-limit cliff not represented by current benchmark gates.
- **Next decision:** add vertex-count scaling benchmarks first. Prefer an existing leaf-owned
  geometry update or eager promotion if it meets the measured bound. Write a dedicated adjacency
  representation ADR only if the chosen fix changes persisted row meaning, scan geometry, or PMA
  ownership; a local bounded optimization needs only a plan plus design/benchmark sync.
- **Related contracts:** [ADR 0001](adr/0001-labeled-segment-slide.md),
  [ADR 0022](adr/0022-degree-driven-hub-edge-storage.md), [storage/lara.md](storage/lara.md)

### GAP-2026-07-31-001 - Prepared sort variants are not retained in the heap cache

- **Status:** Resolved (2026-08-22; bounded heap cache
  `PREPARED_SORTED_CACHE` keyed by `(PreparedPlanKey, PreparedSortSignature)` — the signature is
  validated once by `normalize_prepared_sort` (the single source of truth shared with AST
  injection): direction text canonicalized case-insensitively, allowed keys exact-matched and
  deduplicated, term order preserved because `ORDER BY a, b` and `ORDER BY b, a` are different
  plans. Cap 128 entries with smallest-key eviction on insert; invalidated on upsert replacement,
  `drop_prepared`, and post-upgrade rebuild (variants rebuild lazily). Stable storage keeps only
  query source + metadata per ADR 0053's heap-cache principle, so no stable-layout change.
  Measured `bench_prepared_sorted_variant_rebuild` 318.12K vs
  `bench_prepared_sorted_variant_cache_hit` 29.50K instructions (~10.8x reduction).
  Owning tests: hit-path mutation discriminator (`sorted_variant_cache_hits_normalized_signature`
  fails if the wrapper re-plans), distinct term-order/direction entries, upsert/drop/upgrade
  invalidation, cap eviction, and unchanged validation-error contracts. Router unfiltered
  `canbench --persist` deferred while the crate's benchmark artifact carries a parallel stream's
  in-flight measurements; resume with `cd crates/router && canbench --persist`.)
- **Severity:** P2 prepared-query performance gap
- **Owner:** Router prepared-query heap cache
- **Observed behavior:** The Router keeps the unsorted prepared plan in the heap cache. When a caller supplies a non-empty sort specification, the Router validates the metadata and rebuilds a derived plan for that invocation; equivalent sort specifications are not cached as separate heap entries.
- **Expected or needed behavior:** Repeated executions of the same prepared operation with the same normalized sort specification should reuse a heap-cached derived plan while stable storage continues to retain only the query source and metadata.
- **Evidence:** crates/router/src/prepared.rs::prepare_sorted_cache and the prepared query local E2E in crates/codegen/e2e/test.mjs.
- **Impact:** Sort-enabled prepared queries pay the planning and plan-encoding cost on every execution. This preserves correctness and upgrade safety but can increase latency and instruction usage for frequently reused sort variants.
- **Next decision:** Define a bounded cache key and eviction policy, including direction normalization, key ordering, metadata changes, and post-upgrade invalidation, then add hit/miss tests and a focused benchmark before introducing durable or unbounded derived state.
- **Related contracts:** ADR 0053 and crates/prepared-api/src/lib.rs

### GAP-2026-07-04-001 — Prepared execution still requires graph visibility

- **Status:** Closed by ADR 0063 (2026-08-06)
- **Severity:** P2 product gap; P1 if a public frontend must call Router prepared queries directly
- **Owner:** Router prepared catalog resolution and graph authorization
- **Observed behavior:** `authorize_prepared_execute` permitted the default Router `Executor` role,
  including an anonymous caller, but prepared-plan resolution searched only graphs visible to the
  caller. A principal that is not the graph owner or in the graph `admins` set therefore could not
  resolve the prepared plan. The social demo test had to add its application caller to the graph
  administrators while leaving its Router role at `Executor`.
- **Resolution (ADR 0063, 2026-08-06):** prepared queries are addressed by global name and bound to
  a graph at registration time; `authorize_prepared_execute` and `resolve_prepared_graph_id` were
  removed, so any caller can execute an administrator-registered prepared query. Caller-aware
  authorization (public vs visible graphs, per-query permissions) is deferred to a future
  access-control ADR.
- **Historical evidence:** `crates/router/src/rbac.rs::authorize_prepared_execute`,
  `crates/router/src/prepared.rs::resolve_prepared_graph_id` (both removed by ADR 0063), and the
  shared `install_single_shard_federation_with_graph_admins` fixture in
  `crates/pocket-ic-tests/src/lib.rs`.
- **Historical workaround (removed 2026-08-06):** `crates/social-demo-gateway` was an
  application-owned canister with a fixed scenario enum. The Gateway principal was registered as a
  graph administrator so Router could resolve the prepared plan, but it remained a default Router
  Executor with no ad-hoc `Read` role. Anonymous callers executed the fixed scenarios through the
  Gateway; Router observed the Gateway principal, not the original caller. This was an
  application-layer trusted-deputy pattern, not a product change to Router prepared-query
  authorization. The crate was removed together with the SDK-direct frontend; the frontend now
  calls Router `prepared_query` by name through `@gleaph/sdk`.
- **Related contracts:** [security/rbac-and-prepared.md](security/rbac-and-prepared.md),
  [demo/social-graph-rag.md](demo/social-graph-rag.md)

### GAP-2026-08-01-001 — Social-demo load artifacts were not reproducible from build-config.mjs

- **Status:** Resolved by Plan 0204 on 2026-08-02 UTC
- **Severity:** P2 demo-tooling drift; blocks trusting `pnpm run build:config` as an idempotent
  regeneration step
- **Owner:** `demo/social/scripts/build-config.mjs` and the committed seed/avatar
  artifacts
- **Resolution:** The generator now emits one ordered NDJSON pair (`seeds/vertices.jsonl` +
  `seeds/edges.jsonl`) directly from the canonical
  node/edge model. Its typed properties contain no execution-time values, application endpoint keys
  remain explicit, and exact UTC timestamps are deterministic. Focused contracts cover source-ID
  closure and byte-stable generation inputs; the durable loader uses the full artifact SHA-256 plus
  exact Router chunk replay rather than parsing generated mutation text.

### GAP-2026-08-03-001 — Router seal activation crash window can strand an already-Active Graph export scope

- **Status:** Resolved by ADR 0059 seal-step tolerance (commit in the same patch)
- **Severity:** P2 crash-window recovery gap (narrow, non-blocking for first rollout)
- **Owner:** `crates/router/src/facade/store/schema_migration/driver.rs` seal composition plus Graph
  `canonical_export` Active-scope semantics
- **Observed behavior:** In one seal drive the driver publishes every Graph export scope
  (`admin_activate_index_export_scope`) as its final remote step. If the Router message traps or
  rolls back after those activations but before persisting the `Converged` target, a re-drive's
  step-1 `admin_seal_index_export_scope` returned `InvalidPhase` because the scope is already
  `Active`; the driver classified that as terminal, the router entered `Aborting`, and Graph
  rejected `admin_abort_index_export_scope`/`admin_remove_index_export_scope` for an `Active`
  scope, so cleanup could not progress.
- **Resolution:** `seal_scope` now treats an already-`Active` scope under the exact same frozen
  identity and lifecycle epoch as an exact replay and returns the durable status instead of
  `InvalidPhase`; `activate_scope` already replays an `Active` scope whose proof matches. The
  re-drive therefore re-seals (replay), re-reads the graph-index seal proof (idempotent),
  drains (already converged), and re-activates (idempotent) without error, so the Router
  persists `Converged` and completes normally. A different epoch or identity still fails closed,
  and `Active` remains deliberately non-abortable and non-removable.
- **Evidence:** `crates/router/src/facade/store/schema_migration/driver.rs` `drive_seal`;
  `crates/graph/src/index/canonical_export.rs` `seal_scope`/`activate_scope`/`abort_scope`/`remove_scope`;
  ADR 0059 seal ordering.
- **Regression tests:** `seal_scope_replays_already_active_scope_after_activation_crash_window`
  (graph lib) and `driver_seal_resumes_after_activation_crash_window` (router driver) prove the
  re-drive converges and that a wrong `InvalidPhase` return for the same identity/epoch would fail.

### GAP-2026-08-04-001 — `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` is defined but never consumed

- **Status:** Resolved (2026-08-22; option (b) implemented. The between-chunk loop in
  `execute_prepared_mutation` now runs the ADR 0042 between-wave check before every chunk via
  `graph_batch_chunk_within_update_budget`, composing the crate's generalized `should_cutoff`
  predicate with ceiling `MAX_UPDATE_CALL_INSTRUCTIONS` (40B), the new
  `ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE` (50M: measured worst per-chunk Router work
  37.34M from GAP-2026-07-17-001 evidence plus ~34% margin), and the previously-dead
  `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` (4B) as the finalization reserve — so the constant
  is consumed by the boundary it claims to protect, and the Router provably stops dispatching
  rather than trapping at 40B regardless of Graph behavior. When the guard trips, deferred
  operations surface as per-operation errors ("router update instruction budget guard stopped
  chunk dispatch") through the same recovery path as chunk failures; resubmission completes
  them idempotently via the mutation journal. The live counter comes from the now-ungated
  `current_instruction_counter` (host builds return 0, so native tests exercise the untripped
  path). Owning tests: `chunk_budget_allows_a_fresh_message`,
  `chunk_budget_trips_at_and_past_the_exact_boundary` (pins the exact `>=` trip point against
  dropped/reordered terms or a flipped comparison),
  `chunk_budget_trips_when_only_headroom_remains`. Residual risk: loop wiring itself is not
  natively testable without an inter-canister seam; the pure guard is fully pinned.)
- **Severity:** P2 router budget-integrity gap (no current runtime risk; the shared headroom covers the
  path)
- **Owner:** `gleaph-instruction-budget` constants and the Router chunk-dispatch loop
  (`crates/router/src/gql.rs` `execute_prepared_mutation` / `graph_batch_chunk_len_for_dispatches`)
- **Observed behavior:** `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` (4B) is defined in
  `gleaph-instruction-budget` and re-exported from `gleaph-graph-kernel`, but no code path consumes
  it: the Router chunk decision caps at `max_operation_count(500M, 35B)` = 70 operations per chunk
  using the shared `MAX_DYNAMIC_UPDATE_INSTRUCTIONS`, and the between-chunk loop
  (`execute_prepared_mutation`) has no Router-local call-context cutoff. The 4B constant appears only
  in the definition, the re-export, the compile-time assertion, and the GAP acceptance bench comment.
- **Expected or needed behavior:** every defined headroom must either be consumed by the owning
  boundary or be removed. If the Router is to guarantee it never crosses the 40B limit on its own
  account (distinct from the Graph-side 35B budget inside each dispatched batch), the chunk loop
  needs the ADR 0042 original between-wave check: read the Router call-context instruction counter
  and stop before starting another chunk when `used + next-chunk estimate + reserve ≥ 40B`.
- **Evidence:** `crates/instruction-budget/src/lib.rs` (definition + assert),
  `crates/graph-kernel/src/lib.rs` (re-export), `crates/router/src/gql.rs`
  (`graph_batch_chunk_len_for_dispatches` uses only the shared budget); measured per-chunk decision
  cost 6.01M / 37.34M instructions and the ≤2 MiB inter-canister sizing bound
  (`crates/router/src/bench.rs`, `crates/router/canbench_results.yml`, GAP-2026-07-17-001).
- **Impact:** no current runtime risk: the Router's actual safety is the shared 5B
  `UPDATE_CALL_INSTRUCTION_HEADROOM` (35B dynamic budget), and the measured per-chunk work
  (decision ≤ 37.34M + ≤2 MiB encode + response) is ≪ 1% of it. The gap is integrity and
  future-proofing: the dead constant invites a false sense of protection, a future Router-side
  bounded loop cannot reference a consumed budget, and the GAP acceptance table lists a value that
  is not wired to the path it claims to reserve.
- **Next decision:** choose one of (a) remove the constant and the ADR 0060 §1 list entry, noting
  in GAP-2026-07-17-001 that the shared 5B covers the Router chunk path; or (b) add the
  between-chunk call-context cutoff to `execute_prepared_mutation` and re-derive the constant from
  measurement (≈1B suffices: decision 37.34M + response at ~7× margin). Option (b) is preferred so
  the Router itself provably never traps at 40B regardless of Graph behavior.

### GAP-2026-08-23-001 — Native `gleaph-router` lib suite has pre-existing ic0-context failures that poison unrelated tests

- **Status:** Fully resolved (2026-08-23 build breakage, B1, adr0034 split-out, and ic0-context
  admission deferral; 2026-08-24 var_len group-variable access)
- **Severity:** P3 test-harness (the ic0/build items below were P1 while present)
- **Owner:** `gleaph-router` native unit test harness; `gleaph-graph` expand executor and adr0034 fixture own their respective items

#### Resolved 2026-08-23

1. **Workspace build breakage (8e392c127 → ac380c4bb).** `8e392c127 feat(index): wire edge range pushdown end to end`
   added `collect_edges_matching_indexed_property_where` call sites in
   `crates/graph/src/index/edge_lookup.rs` but omitted the collector's definition in
   `crates/graph/src/facade/store/edge_properties.rs`, leaving every commit from 8e392c127 through
   463478b44 unable to build `gleaph-graph`. Fixed by `ac380c4bb fix(graph): add the predicate-scanned
   edge property collector`.
2. **Directed equality-index candidates lose inline bytes (`b787ac389` → 317517074).**
   `b787ac389 fix(index): own edge posting label identity in wire space` let directed postings reach
   the equality-index candidate path for the first time, but that path read slots through
   `read_out_edge_slots_for_label`, which restores topology only — scanned edges carried empty inline
   property bytes, failing projected inline reads. The LARA slot-targeted reader is topology-only by
   design (`read_edge_state_at_slot` restores no payload); byte restoration lives in the batch walk.
   Fixed by `317517074 fix(graph): restore inline bytes in equality-index candidates`: the PointingRight
   arm now walks the labeled adjacency once with batch-restored bytes and keeps the posted slots,
   mirroring the PointingLeft arm. Owning test: `indexed_edge_equality_expand_return_inline_property`.

3. **var_len group-variable property access under default-enabled cypher (`b13d427ca`) — resolved 2026-08-24.**
   `gql_var_len_where_inline_property_filters_on_last_hop_edge` ("property access on group edge variable
   'e.distance' requires element indexing") and `gql_var_len_return_inline_property_decodes_indexed_last_hop_edge`
   (decodes to Null) were newly *exposed*, not regressed, by the cypher dialect gate (green at
   da665a70b only because the tests did not compile in). Two executor gaps: `e[-1].prop` resolved the
   indexed element through the technical whole-edge record (`edge_to_value`), which carries no decoded
   inline properties, and `Compare` over a direct group-property access had no defined quantifier.
   Fixed by defining both semantics in `QueryExprEvaluator`: indexed elements read their property
   exactly like a single-edge binding (inline decode → sidecar fallback), and a residual group-property
   comparison quantifies over **every** hop — mirroring the schema-fused `edge_inline_property_predicate`
   contract so fused and residual plans stay result-equivalent. Owning tests:
   `gql_var_len_return_inline_property_decodes_indexed_last_hop_edge`,
   `gql_var_len_where_inline_property_filters_on_last_hop_edge`, and the all-hop discriminating
   `gql_var_len_where_group_property_requires_every_hop_to_match`. Contract recorded in
   [group-variables.md](../design/execution/group-variables.md).

#### Resolved 2026-08-23 (ic0 admission deferral)

4. **ic0-context panics poisoned the router lib suite (this host) — resolved 2026-08-23 late UTC by
   deferring the self-principal read.** `cargo test -p gleaph-router --lib` failed three dev-mode
   fail-closed tests in this macOS host environment:
   `facade::store::schema_migration::tests::unregistered_create_graph_migration_fails_closed_without_provisioner`,
   `provisioning::graph::tests::admission_fails_closed_for_unregistered_name_without_provisioner`,
   and `provisioning::graph::tests::admission_short_circuits_registered_name_without_provisioner`,
   all panicking with `canister_self_size should only be called inside canisters` (`ic0` system call
   reached outside canister execution), in full runs and in isolation. Root cause: the
   `create_graph_admission` wrapper evaluated `ic_cdk::api::canister_self()` eagerly as an argument
   to `create_graph_admission_with`, so the ic0 read fired before the provisioner-absent rejection;
   each panic also poisoned the shared outbox/registry test lock, cascading into
   `vector_sync::tests::{frontier_response_loss_retains_exact_marker_snapshot,
   resolved_rows_transition_to_awaiting_frontier_before_publish}`, which pass when run alone.
   Fixed in this entry's closing commit: `create_graph_admission_with` now takes the Router
   self-principal as an `impl FnOnce() -> Principal` and evaluates it exactly once, only after every
   fail-closed rejection has returned; the wrapper passes the `ic_cdk::api::canister_self` function
   path itself. No endpoint check was relaxed and no ic0 stub abstraction was introduced. Owning
   tests: the three admission/migration tests above plus the de-poisoned vector_sync pair.

The adr0034 fixture item that formerly appeared here as "Remaining open" item 5 now lives in its own
entry, [GAP-2026-08-23-002](#gap-2026-08-23-002--adr0034-e2e-fixture-predates-typed-schema-endpoint-enforcement).

#### Evidence summary

- Build breakage and B1: baseline reproduction with working-tree changes stashed at HEAD 463478b44;
  worktree bisect across 8e392c127 / cb156b0b7 / b787ac389 / b6fe10612 with the missing collector copied
  forward; candidate-path disable experiment turned B1 green, confirming the reader as the defect site.
- ic0 panics reproduce on clean 463478b44; isolated vector_sync runs are green.
- adr0034 failure persists with the slice-4 planner changes reverted, so planner anchor selection is not
  involved; see GAP-2026-08-23-002 for its owning entry.

### GAP-2026-08-23-002 — ADR 0034 E2E fixture predates typed-schema endpoint enforcement

- **Status:** Resolved (2026-08-23) — both `adr0034` read fixtures were reproduced against the
  typed-schema endpoint gate, aligned with their declared graph types without weakening validation,
  and rerun green (struct in `8f3f5c1da`; scalar in the closing commit).
- **Severity:** P1 E2E correctness-gate blocker (keeps the adr0034 PocketIC target red); not a
  production storage/query defect.
- **Owner:** the stale `adr0034` read fixtures, which insert vertices whose labels contradict their
  declared graph types. The typed-schema endpoint check promoted at Router ingress is correct
  fail-closed behavior and must not be weakened. Planner anchor code is not involved.

#### Observed behavior

`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_struct_read_access` failed at HEAD
`9a4b94892` (and on the clean slices 1–3 baseline `463478b44`) with:

```
gql_query rejected: InvalidArgument("validation error: edge `:AFFINITY` cannot connect :ProjectionSource to (unlabeled) (schema constraint violation)")
```

The DDL declares `NODE User AS user ... CONNECTING (user -> user)`, but scenarios inserted sources
labeled `ProjectionSource`/`FilterSource`/… and unlabeled targets, so every MATCH violated the
declared endpoint set. `cargo test -p gleaph-pocket-ic-tests --test
adr0034_inline_edge_scalar_access` fails identically for `:ROAD` over `road_type`.

#### Fix evidence (2026-08-23)

`crates/pocket-ic-tests/tests/adr0034_inline_edge_struct_read_access.rs` was aligned with its
declared graph type: vertices now carry the declared `User` label, and per-scenario independence
moved from ad-hoc source labels to unique `updated_at` windows scoped through WHERE clauses. The
endpoint validator itself is unchanged. Terminal rerun:
`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_struct_read_access` → **1 passed /
0 failed**, one single-shard federation constructor, three canister installs.

The sibling scalar fixture received the same alignment in the closing commit: vertices carry the
declared `City` label, per-scenario independence moved from ad-hoc source labels to disjoint
`distance` bands (7 / 17·19 / 27·29 / 37) scoped through WHERE equality and band-range predicates,
and every row-count/order assertion keeps its original intent. The scalar expectations were also
corrected from the never-exercised `Uint64` decode to the declared-type `Uint16` wire value,
mirroring the struct fixture's `Float64` → `Float32` correction in `8f3f5c1da`. Terminal rerun:
`cargo test -p gleaph-pocket-ic-tests --test adr0034_inline_edge_scalar_access` → green after a
fresh re-run immediately before judgment (shared-target contention with parallel agents produced
one transient unrelated build failure).

#### Next decision

None — closed. Both adr0034 read fixtures now conform to their declared graph types, and the
typed-schema endpoint gate remains unchanged and fail-closed.

## Resolved gaps

### GAP-2026-09-02-001 — RESOLVED: production `LabeledLaraGraph::visit_edges` dispatches tree-mode buckets into the dense slab read path

- **Status:** Resolved (2026-09-02, Plan 0324 REWORK-2).
- **Severity:** P0 production-correctness defect (read of stable
  memory garbage / trap with `VectorMemory::read: out of bounds`).
  Tree mode is the primary high-degree storage mode; any production
  canister that visits a high-degree tree bucket would trap.
- **Root cause:** 3 dispatch sites in `traverse.rs` used a "dense,
  tombstone-free" fast path that bulk-reads
  `degree × E::BYTES` bytes from `bucket.edge_start()`. For tree
  mode, `edge_start` is the LEG root region (block_id array,
  `root_len × 4` bytes), not the LTB payload blocks. The dispatch
  condition matched tree mode (overflow_log_head = -1,
  reserved_slots = stored_slots), so the dense slab-path was
  incorrectly taken and the bulk read went OOB.
- **Initial fix (REWORK-2)**: tree branches routed through
  `tree_mode_out_edges_collect` (Vec materialization) + `enumerate()`,
  yielding live-ordinal positions. Worked for dense tree buckets
  (where enumerate == tombstone-inclusive slot) but violated the
  position contract (ADR 0088 §2) for tombstoned buckets.
- **Final fix (REWORK-3, this slice's design)**: tree branches call
  `visit_tree_mode_label_bucket_edges` directly, forwarding the
  tombstone-inclusive `u32` slot to `BucketEntryPosition::new(slot)`.
  `ControlFlow::Break` is captured via a mutable cell. Dispatch
  order reordered: dense condition first (more selective, with
  `!is_tree_mode()` guard), tree branch after. **~50% instruction
  reduction on the 3 read benches** as a bonus.
- **Fix (REWORK-3):** added `if bucket.is_tree_mode() { ... }` branches
  at 3 dispatch sites:

  | Site | Function | Routing |
  |---|---|---|
  | `traverse.rs:790` | `visit_edges` | `tree_mode_out_edges_collect` + `ControlFlow` adapter |
  | `traverse.rs:1580` | `visit_edges_window` | `tree_mode_out_edges_collect` + window slice + `ControlFlow` adapter |
  | `traverse.rs:1843` | `visit_edges_with_inline_property` | `!is_tree_mode()` guard on dense path; falls through to `visit_slab_bucket_span_edges` (LTB-aware) |

  `visit_edges_for_label_impl` (line 2000) and
  `visit_edges_for_label_with_inline_property` already had the
  tree branch (Plan 0318 Step 5). Other read paths require
  `inline_property_byte_width > 0` for dispatch (a precondition),
  which is always 0 in tree mode (set on promote), so they are
  tree-mode safe by construction.

- **Regression tests:**
  - `tree_visit_edges_via_public_api_works_on_dense_tree_bucket` in
    `tree_read.rs:447` (dense tree bucket: 4096 slots, no tombstone).
    Verified to FAIL without the fix and PASS with the fix.
  - `tombstoned_tree_bucket_visit_preserves_logical_positions` in
    `tree_read.rs:511` (REWORK-3): promotes 4K tree bucket,
    tombstones slot 100, verifies `graph.visit_edges` yields 4096
    visits (not 4095) with tombstone at position 100, all positions
    0..4096 covered exactly once, ascending AND descending.
    Verified to FAIL with the REWORK-2 fix (enumeration gives
    live-ordinal; tombstone at position 3995 in descending) and
    PASS with the REWORK-3 fix (tombstone at position 100 in both
    orders).

- **Bench verification:** the 3 previously-TRAPping benches
  (`tcsr_1048576_full_scan_descending`,
  `tcsr_1048576_random_ordinal_access`,
  `tcsr_131072_full_scan_descending`) now PASS deterministically
  (3-for-3 each) and are persisted in `canbench_results.yml`.
  Original 3 write-path benches (131K insert_grow, 1M insert_grow,
  1M root_capacity) still PASS. 4K/65K prototype baseline 0%
  regression (6/6 unchanged). Full canbench suite 158/158
  unchanged, 0 regressed, 0 failed.

- **Why the bug wasn't caught in Plan 0318 Step 5:** the Step 5
  read-dispatch tests used `tree_mode_out_edges_collect` directly
  (the LTB primitive), not the public `graph.visit_edges` API.
  The public API has its own dispatch at `visit_edges` line 786
  that unconditionally tried the dense fast path before falling
  through. The Step 5 tests also used bucket degrees below the
  LEG root size (4096 edges = 4 LTB blocks = LEG root 16 bytes),
  so even a misread from `edge_start` returned within the (small)
  memory window without trapping. The 1M bench exposed the bug
  because 1M = 1024 LTB blocks = LEG root 4096 bytes, while the
  dense read attempted 4MB.

- **Diagnostic history:** the original (REWORK-1) audit
  misdiagnosed the bug as "seeding above ~77K edges traps non-
  deterministically" and proposed reducing the bench anchor to
  131K. The REWORK-1 audit was internally inconsistent (the
  131K insert_grow bench worked, requiring seeding past the
  alleged 77K boundary). The REWORK-2 audit identified the
  read-dispatch as the actual cause; REWORK-2 verified
  determinism (3-for-3 trap) and re-root-caused the OOB.

### GAP-2026-09-02-002 — RESOLVED: `tree-mode-interior-level-insert-growth` (right-spine cascade) and the 2^30 fail-closed boundary

- **Status:** Resolved (2026-09-02, Plan 0325).
- **Severity:** P1 production-correctness defect (effective tree-mode
  cap of 2^20 per bucket was 4 KiB per label per vertex — too tight
  for any non-trivial high-degree use case).
- **Owner:** `tree_mode_insert_edge` (`tree_write.rs:58-280`),
  `tree_mode_deepen` (already shipped, level-generic), depth-generic
  resolver `resolve_leaf_block_id` (already shipped, physical depth).
- **Observed behavior:** the Plan 0318 §Step 7 amend shipped an
  interim fail-closed `TreeRootCapacityReached` guard at 2^20
  (`stored_slots + 1 > R_MAX × B = 1,048,576` in a depth-1 tree
  bucket). Production tree-mode inserts above 2^20 returned
  `Err(TreeRootCapacityReached)`. The depth-generic infrastructure
  needed for the cascade was already shipped and unit-verified
  (mixed-radix resolver, `tree_mode_deepen`, `tree_mode_flatten`,
  demote Phase 5b interior release, `bucket_span_region_len`
  physical-depth match) — only the insert-side wiring was missing.
- **Why it matters (design invariant):** ADR 0088 §4 documents
  `TREE_STRUCTURAL_CAP = 2^30` (depth 1 → 2 at 2^20, depth 2 → 3 at
  2^30, MAX_DEPTH = 3 primitive safety). The interim guard
  contradicted the documented cap by 2^10. Production canisters
  promoting high-degree label buckets to tree mode would hit the
  guard at 2^20 with no way to grow further.
- **Fix (Plan 0325):** rewrote `tree_mode_insert_edge` to wire
  the right-spine cascade:
  1. The `tail_offset == 0` branch now consults the
     **physical** root length (ceil-chain, NOT
     `derived_root_len` — `derive_depth` returns 1 for a deepened
     bucket at stored = 2^20, but the physical root has 1 entry,
     not 1024).
  2. When the physical root is at `R_MAX`, the cascade fires:
     - if `depth >= MAX_DEPTH` OR `next_stored > TREE_STRUCTURAL_CAP` →
       typed `TreeRootCapacityReached` (fail-closed at the 2^30
       structural boundary);
     - else → `tree_mode_deepen` + **re-read the bucket
       descriptor** (the caller's `bucket` copy is stale after
       deepen publishes a new `edge_start` /
       `tree_mode_physical_depth`).
  3. After the cascade, the dispatch routes to
     `tree_mode_tail_append_depth1` (depth 1) or
     `tree_mode_tail_append_depth_ge2` (depth ≥ 2). The depth ≥ 2
     path appends the new leaf id to its **home interior** (not
     the root); the root grows only when a new interior is minted
     (`l % K == 0`).
  4. New helper `resolve_interior_block_id` — the
     `resolve_leaf_block_id` hop chain truncated one level short.
     Used by `tree_mode_tail_append_depth_ge2` to find the
     home interior.

- **Audit of depth-generic infrastructure (file:line):**
  - `resolve_leaf_block_id` (tree_write.rs:516) — depth-generic ✓
  - `collect_leaf_block_ids` (tree_write.rs:586) — depth-generic ✓
  - `tree_mode_deepen` (tree_write.rs:632) — level-generic ✓
  - `tree_mode_flatten` (tree_write.rs:830) — depth-2 → depth-1 ✓
  - demote Phase 5b (tree_write.rs:1025) — interior release ✓
  - `bucket_span_region_len` (compact.rs:234) — physical-depth match ✓
  - `tree_mode_random_ordinal_access` (tree_read.rs:54) — DEPTH-1
    DEBUG_ASSERT was relaxed (the prior `block_root_index < root_len`
    debug_assert used structural root_len; relaxed to
    `block_root_index < leaf_count` for depth-2 safety)
  - `visit_tree_mode_label_bucket_edges` (tree_read.rs:115) —
    DEPTH-1 DEBUG_ASSERT was relaxed (the prior
    `debug_assert_eq!(leaf_count, root_len)` failed for depth ≥ 2;
    relaxed to `leaf_count >= root_len` invariant — actual walk
    uses `resolve_leaf_block_id` for the descent)
  - `tree_mode_insert_edge` `tail_offset == 0` branch
    (tree_write.rs:113-280) — **DEPTH-1-ONLY** (replaced by the
    cascade + depth-aware append)

- **Regression tests** (synthetic layout, host `VectorMemory`,
  cheap):
  - `tree_insert_fails_closed_at_2_30_cap`: 2^30 fail-closed at
    depth 2 + root_len = R_MAX.
  - `production_insert_path_fails_closed_at_2_30_cap`: same,
    via the public `insert_edge_skip_leaf_cascade` API.
  - `cascade_at_2_20_plus_1_deepens`: 2^20 + 1 insert SUCCEEDS,
    depth 1 → 2, stored 2^20 + 1, visit yields 2^20 + 1 slots.
  - `interior_row_append_keeps_root_constant`: depth 2 with
    2 interiors + 1 leaf in the 2nd; next insert at row 1 of the
    2nd interior (l = 1025, l % K = 1) — root unchanged.
  - `new_interior_mint_grows_root`: depth 2 with 2 full interiors
    + tail_offset == 0; next insert at l = 2048, l % K = 0 —
    root grows from 2 to 3.
  - `public_read_accessors_over_depth_2`: `tree_mode_out_edges_collect`
    walks 1,048,577 slots for stored = 2^20 + 1, depth 2.
  - Replaces the obsolete 0318 §Step 7 tests
    `tree_insert_fails_closed_at_root_capacity` and
    `production_insert_path_fails_closed_at_root_capacity` (the
    2^20 guard was superseded by the cascade + 2^30 boundary).

- **Bench surface:** REPLACED `tcsr_1048576_root_capacity_reached`
  (2^20 + 1 insert previously FAIL-CLOSED) with
  `tcsr_1048576_deepen_beyond_r_max` (2^20 + 1 insert now
  SUCCEEDS via deepen; verified PASS ×3 with depth 2 / root_len 2 /
  stored 2^20 + 1 / edge readable at slot 2^20). ADDED
  `tcsr_1048576_deepen_then_interior_grow` (2^20 → 2^20 + 1024:
  1 deepen + 1023 interior-row appends + 1 new-interior mint;
  5,111,272 ins, 4994 ins/insert).
  - Existing depth-1 benches (5 of them) 0% regression
    (insert_grow / full_scan / random_ordinal / 131K insert_grow
    / 131K full_scan): "no change" or within noise threshold
    (≤ 1% on insert_grow, 0% on read benches).
  - Full canbench 158/158 unchanged, 1 regressed
    (`bench_t_v_window` +2.10%, pre-existing from Plan 0324
    REWORK-3; small slab bench, 220 ins regression, acceptable).
  - Wasm 15,661 chars / 4,339 headroom (was 15,536; +125 for
    2 new bench names).

- **Why depth 3 is not exercised (deliberate):** ADR 0088 §4
  documents `MAX_DEPTH = 3` as a primitive-level safety bound
  (`TreeDepthLimitReached`); the production cap of 2^30 binds
  first. Depth 3 coverage (2^40 slots, 1 TiB per bucket) is
  structurally wired (the resolver handles it, the canbench
  surface cannot seed it: 17.9T ins > 10T per-bench limit). Lifting
  the cap to 2^40 in production requires an ADR amend (out of
  scope for this slice).

### GAP-2026-08-28-001 — RESOLVED: index-anchored scans are unattributed in authz requirement extraction (grants silently stop working when an index exists)

- **Status:** Resolved (2026-08-28). Fixing commit `0aebf4513`; Router suite 1045 → 1056 green.
- **Severity:** P1 authorization-semantics defect (availability + plan-dependence)
- **Owner:** Router authz requirement walker (`crates/router/src/authz.rs` `walk_op`,
  `PlanCatalogView`, `RouterCatalogView`) with the index catalog seam
  (`crates/router/src/facade/stable/indexed_catalog.rs`)
- **Observed behavior:** On the demo network — the one environment with an active property
  index (`document_title`, demo migration 000002) — every index-anchored read was denied to
  non-tenants regardless of grants: `MATCH (d:Document {title: 'X'}) RETURN 1` → uniform
  `Forbidden` for anonymous, while the identical shape over the deliberately unindexed
  `Concept.name` executed, and the registry owner (tenancy root) always passed. The walker's
  `PlanOp::IndexScan` arm deferred the anchor's property read without noting any label fact;
  an index anchor never receives a later `NodeScan`, so the deferred read resolved with
  `unique_vertex_label == None` → `require_unattributed` → fail-closed tenancy-only. The
  same gap existed in the `IndexIntersection` arm and the `ConditionalIndexScan` candidate
  path (only `fallback_label` was noted). Every extraction contract test ran indexless
  fixtures (ADR 0054 indexless bootstrap), so the planner always emitted `NodeScan` and the
  `IndexScan` arms had zero walker coverage — the bug was invisible to the suite and only
  the live demo network surfaced it.
- **Why it matters (design invariant):** authorization demands must not depend on which
  physical plan the planner chooses. Before the fix, creating an index silently changed who
  could run an existing prepared query — physical layout leaking into logical access
  control. Removing the demo's index would have hidden the defect, not fixed it.
- **Implemented behavior:** `PlanCatalogView` gained a catalog-driven label resolution for
  the unique active vertex property index covering a property (same unique/fail-closed
  semantics as the executor's `active_vertex_physical_index`); the `IndexScan`,
  `IndexIntersection`, and `ConditionalIndexScan` walker arms now note the resolved label
  fact and demand the same scan-side rows an equivalent `NodeScan` would (`Match` +
  `Read`/`ReadProperty` via `require_vertex_scan_rows`). Ambiguous or inactive index
  resolution stays unattributed (fail-closed, unchanged). The PocketIC citation-reach flow
  fixture now creates `document_title` and drains maintenance to active, pinning the
  index-anchored path end-to-end: a non-owner executes the op after the same PUBLIC grant
  surface.
- **Evidence:** live bisection on the demo network (indexed `Document.title` filter denies,
  unindexed `Concept.name` filter passes; plain scans, traversal, quantified paths, and
  `ELEMENT_ID` projections all pass) + indexless-vs-indexed extraction tests in
  `crates/router/src/gql_grants.rs` and walker unit tests in `crates/router/src/authz.rs`.
- **Next decision:** none open for the walker. Long-term, the planner could carry the label
  in `PlanOp::IndexScan` (self-describing plan) — planner-stream territory, deferred until
  the next plan-wire revision; the catalog-driven resolution matches executor semantics and
  needs no wire change.

### GAP-2026-08-26-001 — RESOLVED: `ELEMENT_ID` over quantified-path group variables fail-closed as "requires element indexing"

- **Status:** Resolved (plan 0307 slice A, 2026-08-26) — executor completeness for GQL
  group variables, not a demo workaround.
- **Severity:** P2 demo-blocking executor gap
- **Owner:** graph executor expression evaluation
  (`crates/graph/src/plan/query/executor/eval.rs`, `eval_element_id`)
- **Observed behavior:** With the PUBLIC grant surface applied, the knowledge demo's
  `citation-reach` op (`ELEMENT_ID(e)` over `-[e:CITES]->{1,3}`) failed at execution for
  ANY caller (owner included) with `InvalidArgument("invalid query expression value for
  'ELEMENT_ID(e) on a group edge variable requires element indexing'")`. Quantified-path
  variables bind as groups (`PlanBinding::EdgeGroup` hop trail / `VertexGroup`), and
  `eval_element_id` rejected all three group binding kinds since commits `51ba273c9` /
  `c7a6eb01a` (2026-06-08). Authorization was never the blocker: requirement extraction
  adds no demand for element-id reads (plan 0306 contract test
  `edge_element_id_projection_demands_stay_attributed`).
- **Expected or needed behavior:** GQL group variables are lists; an element-identity read
  over them returns a list of element ids in group order (empty group → empty list), so
  published ops projecting edge identity execute end-to-end for authorized callers.
- **Resolution:** `eval_element_id` now evaluates `EdgeGroup` → `Value::List` of edge-id
  bytes in traversal order and `VertexGroup` → list of vertex-id bytes in group order,
  reusing the singleton-arm encodings verbatim; empty groups yield empty lists;
  `PathGroup` stays fail-closed with an actionable message (paths are not elements; use
  `CARDINALITY(p)` or path element access). No planner, wire, rkyv, or authz changes.
  Tests: host-level `element_id_on_edge_group_lists_hop_ids_in_traversal_order`,
  `element_id_on_vertex_group_preserves_order_and_empty_group_yields_empty_list`,
  `element_id_on_path_group_stays_fail_closed_with_guidance`; PocketIC
  `knowledge_demo_citation_reach_flow.rs` upgraded to full row-content assertions
  (default-deny leg preserved; non-owner execution with per-row hop-count-pinned
  `cite_edge_id` lists).
- **Evidence:** detection run during brief #2 PocketIC validation (w1:p8, 2026-08-26);
  root cause read of `eval.rs` / `expand/var_len.rs`
  [group-variables.md](execution/group-variables.md); post-fix green runs recorded in the
  Validation Transcript of `plans/0307-group-element-id.md`.
- **Related contracts:** GAP-2026-08-24-008 Impact follow-up (a) — citation-reach runtime
  execution is now actually proven; the 2026-08-26 plan 0306 note was extraction-correct
  but execution remained blocked until this slice.
- **Detection:** w1:p8 during brief #2 validation (2026-08-26); resolved same day by w1:p8
  under brief #3.

### GAP-2026-08-25-004 — Committed HEAD fails `gleaph-router` lib-test standalone: `GqlQueryResult.truncated` constructor updates left uncommitted

- **Status:** Resolved by commit 5cb98eb4c (2026-08-25; landed by w1:pM after the owning pane
  could not be identified and the fix was routed through the operator)
- **Severity:** P1 repository integrity
- **Owner:** `crates/router/src/federation/aggregate_index_fast_path.rs` constructor call sites
  for the `truncated` field added to `GqlQueryResult` by kernel commit `49f10d461` (ADR 0078)
- **Observed behavior:** An isolated worktree at `d31909605` — and equally at then-HEAD —
  failed `cargo test -p gleaph-router --lib` with
  `E0063: missing field 'truncated' in initializer of GqlQueryResult`
  (`aggregate_index_fast_path.rs:120/139/254`). The main tree compiled only while the owning
  stream's uncommitted WIP stayed present, so the breakage was invisible locally.
- **Expected or needed behavior:** Kernel-side field additions and their constructor-site
  updates land in the same commit (GAP-2026-08-24-007 precedent).
- **Resolution:** All three aggregate fast-path result constructors initialize
  `truncated: None` (commit `5cb98eb4c`). Isolated-worktree verification at the fixing commit:
  `cargo test -p gleaph-router --lib` → 980 passed / 0 failed, including the ADR 0081 Slice A
  router-side ordered-range seed fail-closed surface.
- **Evidence:** `git log -S "truncated" -- crates/graph-kernel/src/plan_exec.rs` → `49f10d461`;
  isolated-worktree build log; fixing commit `5cb98eb4c`.
- **Related contracts:** [ADR 0078](adr/0078-authz-aware-vector-search.md)
- **Detection:** w1:pM during ADR 0081 Slice A verification (2026-08-25).

### GAP-2026-07-14-001 — Ordered incremental slab compaction repeatedly scans packed prefixes

- **Status:** Packed-prefix rereads addressed in cursor-forwarding drivers by Plan 0201
  (ADR 0052 Slice 6); quadratic tombstone-gap rescanning remains open.
- **Severity:** P2 maintenance-performance risk
- **Owner:** `ic-stable-lara` labeled edge-slab compaction and deferred-maintenance cursor state
- **Historical observed behavior:** `compact_vertex_edge_span_one_step` preserves edge scan order and emits at
  most one `EdgeSlotMove` per queue pop, but `first_edge_slot_move_in_bucket` restarts at slot zero
  after every move. Alternating tombstones therefore make full bucket compaction quadratic in the
  resident slab width. After edge-log and inline-property maintenance were separated, canbench measured
  `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_1024` at 73.36M instructions versus
  4.14M previously; the existing 4,096- and 16,384-edge cases were already dominated by the same
  quadratic shape.
- **Expected or needed behavior:** preserve ascending/descending edge scan order and immediate
  per-edge sidecar/index re-keying while carrying enough progress state that successive bounded
  maintenance steps do not rescan the already-packed prefix.
- **Resolution:** `CompactVertexEdgeSpan` (v1 work item) carries a `resume_slot_index` cursor;
  `compact_vertex_edge_span_one_step` and both finders scan from the cursor with `next_live` seeded
  from it (packed-prefix invariant), `EdgeMoved` re-enqueues carry the advanced cursor,
  `AdvanceBucket` resets it, and a cursor-scan miss triggers a full re-scan from slot zero before
  `stored_slots = degree`, so an interleaved delete inside the already-packed prefix can never
  truncate a live edge. This does not establish linear work: forwarding avoids rereading the
  packed live prefix but still scans an expanding tombstone gap. The earlier claim that this
  moves compaction off the quadratic shape is withdrawn. The unchanged 16K benchmark itself
  supplies zero; the separate completed zero/forwarded diagnostics and subsequent bounded-read
  improvement are source-tagged in the [follow-up evidence](evidence/adr0097-left-pack-bounded-reads.md).
  Batching reads reduces a constant factor, not repeated decoding.
- **Evidence:** `crates/ic-stable-lara/src/labeled/graph/compact.rs::first_edge_slot_move_in_bucket`
  (cursor parameter), `compact_vertex_edge_span_one_step`; regression tests
  `compaction_cursor_packs_alternating_tombstones_across_steps` and
  `compaction_cursor_survives_interleaved_delete_in_packed_prefix` in
  `labeled/bidirectional/deferred.rs`; ADR 0020's one-`EdgeMoved`-per-pop contract.
- **Related contracts:** [ADR 0001](adr/0001-labeled-segment-slide.md),
  [ADR 0020](adr/0020-deferred-maintenance-timer-drain.md),
  [ADR 0052](adr/0052-per-label-adjacency-order-and-tombstone-reuse.md) Slice 6

### GAP-2026-07-04-003 — No application-facing vertex-embedding ingestion boundary

- **Status:** Resolved by plan 0048 implementation
- **Severity:** P1 product gap
- **Owner:** Router authorization/resolution + Graph validation boundary + Vector durable store
- **Observed behavior:** before plan 0048, there was no canister API for an application or deployment
  tool to write a canonical vertex embedding. Vector-index fixtures and demos had to seed the
  derived `graph-vector-index` canister directly, bypassing Graph canonical ownership and the
  Router embedding-name catalog.
- **Expected or needed behavior:** an authorized caller should submit only graph name, opaque encoded
  vertex id, registered embedding name, and finite F32 values to the typed Router endpoint; Router
  should validate the value dimension/finiteness before stamp allocation and the Graph await, Graph
  should validate vertex/tombstone/label and payload-independent embedding metadata, and every
  allocated Router stamp should have one exact durable owner before the first Graph await.
- **Resolution:** The typed Router `ingest_vertex_embeddings` flow validates the encoded id, live
  shard, registered
  vector definition, value dimension, and finiteness, then synchronously co-writes the allocated
  stamps and exact `AwaitingGraph` intents before the first Graph await. Graph `stamp_embedding`
  validates existence/tombstone state, required labels, and payload-independent embedding
  metadata/encoding, then returns the Router-issued stamp without embedding-byte or journal writes.
  Exact acceptance transitions the row to `AwaitingVector`; exact rejection changes the row to
  `AwaitingFrontier`; an observed Vector prefix also changes only the applied rows to
  `AwaitingFrontier`. Unknown Graph, Vector, or frontier outcomes retain the applicable phase. No
  finite-time or client-level exactly-once guarantee is implied.
- **Evidence:** `crates/router/src/api/control.rs::ingest_vertex_embeddings`;
  `crates/graph/src/canister/handlers.rs::stamp_embedding`; and the focused Router/Vector unit
  coverage. The named PocketIC gate `graph_only_markerless_lane_advances_frontier_on_autonomous_timer`
  passes exactly one test with seven filtered, one `PocketIc`, one federation, and four canister
  installs; it advances the ordinary timer with an empty MemoryId 53 lane and exercises bounded GC.
  The persisted Router `crates/router/canbench_results.yml` artifact has all 41 terminal benchmark
  entries; `markerless_frontier_catalog` records exactly 52,080,138 total and 52,079,143 scoped
  instructions. The dated Plan 0277 focused/persisted Vector `bench_router_frontier_gc_budget`
  baseline records 2,110,300,092 total and 2,110,299,087 scoped instructions. The later live
  artifact currently records 2,084,327,695 total and 2,084,326,690 scoped instructions from
  unrelated work and is not Plan 0277 evidence. The targeted
  affected-crate format check passes, while the
  workspace-wide format check is blocked by unrelated dirty paths and never passed. The bounded
  Quint model has fifteen deterministic tests and 5,000 sampled traces with all requested witnesses;
  `quint verify` remains unrun and non-required, so this is not production proof or a finite-time
  liveness claim.
- **Related contracts:** [ADR 0031](./adr/0031-vertex-embedding-store-and-derived-vector-index.md),
  [design/index/vector-index.md](./index/vector-index.md),
  [design/execution/pipeline.md](./execution/pipeline.md)

### GAP-2026-07-04-002 — `NEXT INSERT` lost edge endpoint identity

- **Status:** Resolved by commit `27e993ae`
- **Severity:** P1 correctness defect
- **Owner:** GQL block planning and Graph projection/mutation execution
- **Observed behavior:** a `MATCH ... RETURN ... NEXT INSERT (a)-[:L]->(b)` mutation reported
  success, but a later traversal observed disconnected/`NULL` endpoints. Separate seed operations
  could not build a shared-vertex social graph.
- **Resolution:** no-YIELD `NEXT` boundaries now preserve typed graph bindings; already-bound node
  variables are not planned as new vertices; plain-variable projections retain `PlanBinding`
  identity through native and wire execution.
- **Evidence:**
  `gql_run::tests::{block_match_next_insert_edge_keeps_endpoints,wire_block_match_next_insert_edge_keeps_endpoints,block_match_next_insert_edge_shares_source}`.
- **Related contracts:** [gql/plan-format.md](gql/plan-format.md),
  [execution/pipeline.md](execution/pipeline.md)

### GAP-2026-07-17-001 — Dynamic instruction-budget headrooms lack measured acceptance criteria

- **Status:** Resolved by measured canbench + PocketIC acceptance (2026-08-03/2026-08-04; commits
  `test(graph): measure plan-batch tail costs for headroom acceptance`, `test(graph): verify
plan-batch drain boundary end to end`, `test(graph-index): measure posting_batch loop costs for
headroom acceptance`, `test(vector-index): measure vector_sync_batch loop costs for headroom
acceptance`, `test(graph-index): measure batched page-answer lookahead tail`, `test(router):
measure graph batch chunk decision costs for headroom acceptance`, `test(graph): measure timer
maintenance tick and per-step costs`)
- **Severity:** P1 availability and resumability risk
- **Owner:** Router, Graph, graph-index, and graph-vector-index dynamic batch boundaries
- **Observed behavior:** Multiple canister paths stop before the 40B update-call limit using
  independently chosen values, but no repository benchmark or acceptance table establishes that
  each headroom covers its owning path's final operation, response encoding, post-operation drain,
  and cross-canister call overhead. Current examples include Router's 5B headroom, Graph's pending
  5B headroom for `execute_plan_update_batch`, graph-index and graph-vector-index batch ceilings at
  32B with 100M per-loop reserves, graph-index's 1B update / 0.5B query lookahead, and Graph timer
  maintenance at 32B with a 100M reserve. These values have different ownership and semantics and
  must not be treated as interchangeable.
- **Resolution:** every dynamic batch / bounded maintenance loop in the GAP now has measured,
  path-specific instruction-ceiling and headroom acceptance evidence (plan-batch, posting_batch,
  vector_sync_batch, page-answer lookahead, Router chunk decision, timer maintenance):
  - **Graph plan-batch** (`execute_plan_update_batch`, `GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM`
    2B + 500M drain estimate): adversarial single operation 32.98M < `MIN_OP_INSTRUCTION_ESTIMATE`
    (50M) and far below the ~37.5B trap threshold; response tail ≤ 581K vs the 2.5B reserve; the
    inter-canister drain is covered by the PocketIC boundary test
    (`adr0060_plan_batch_instruction_boundary.rs`) which completes a 300-operation NEXT-chained
    mutation on a converged index without trapping and serves the drained posting.
  - **graph-index `posting_batch`** (32B ceiling, 100M reserve): max single op ≈1.92M (4096-byte
    value) + response 38K, two orders below the reserve; ceiling cuts off only after ~16.6K
    max-value postings. Fully local; no boundary test needed.
  - **graph-vector-index `vector_sync_batch`** (32B ceiling, 100M reserve): max single op ≈1.12M
    (flat in dims) + response 38K, two orders below the reserve; ceiling cuts off after ~29K
    upserts. Fully local; no boundary test needed.
  - **graph-index page-answer lookahead** (500M query lookahead): max per-check work (worst page
    ≈206.4M + encode ≤ 316K) ≈206.7M, 2.4× below the lookahead. The 1B update lookahead has no
    caller and needs no evidence until used.
  - **Router mutation-batching chunk decision** (`ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` 4B):
    decision cost 6.01M nominal / 37.34M adversarial (≈0.15% / ≈0.9% of the reserve); the
    remaining per-chunk tail is bounded by the inter-canister sizing policy (≤2 MiB) and the
    dispatch count is bounded by live shard count, so no dedicated boundary test was added.
  - **Graph timer maintenance** (`MAX_TIMER_MAINTENANCE_INSTRUCTIONS` 32B,
    `TIMER_MAINTENANCE_INSTRUCTION_HEADROOM` 100M): single work item 61.60K and whole tick on a
    dense 2048-edge hub 1.33M, so the worst per-check work (≤ whole tick) is ≈1.3% of the reserve
    and a realistic backlog drains in ≈0.004% of the cap. Fully local; no boundary test needed.
    Follow-up refactor (`relocate_edge_properties_for_move`): the inline-property posting rekey,
    previously a post-pass loop outside the budget check, now runs inside the per-move observer
    through the unified property relocation, so the entire edge-move consequence is covered by
    the per-item cutoff; the measured tick is unchanged (1.33M, re-persisted 2026-08-04).
- **Evidence:** canbench targets and persisted results: `crates/graph/src/bench/plan_batch.rs`,
  `crates/graph/src/bench/timer_maintenance.rs`, `crates/graph-index/src/bench.rs`,
  `crates/graph-vector-index/src/bench.rs`, `crates/router/src/bench.rs`, with the matching
  `canbench_results.yml` per crate; the plan-batch PocketIC boundary test
  `crates/pocket-ic-tests/tests/adr0060_plan_batch_instruction_boundary.rs`. No headroom was
  justified by copying another canister's constant: each path carries its own measured worst-case
  and tail numbers.
- **Related contracts:** [ADR 0041](adr/0041-router-graph-batch-mutation-dispatch.md),
  [ADR 0042](adr/0042-router-dynamic-instruction-budget-batching.md),
  [ADR 0020](adr/0020-deferred-maintenance-timer-drain.md),
  [design/index/property-index.md](index/property-index.md),
  [design/index/vector-index.md](index/vector-index.md)

### GAP-2026-08-07-001 — Newer-incarnation replacement can tombstone the old row before a fallible append

- **Status:** Resolved by a commit-before-tombstone reorder in `vector_upsert` (Plan 0233).
- **Severity:** P2 correctness edge (only reachable on stable-memory OOM during a subject-map commit)
- **Owner:** graph-vector-index (`VectorCanisterStore::vector_upsert`)
- **Observed behavior:** In the `mutation_id > stamp` (newer-incarnation) branch, the superseded old
  active/shadow slots were tombstoned _before_ the fallible `VECTOR_SUBJECT_TO_ID` commit. If that
  commit failed (`StableGrowFailed`, stable-memory OOM), the retained old subject entry pointed at a
  now-tombstoned row, ghosting the subject from search.
- **Resolution:** The branch now commits the subject map (pointing at the new slot) before tombstoning
  the old slots; on a commit failure it tombstones only the just-appended rows, leaving the old slot
  live and the retained entry consistent. A regression test
  (`newer_stamp_upsert_commit_failure_keeps_old_slot_live`) injects a subject-map insert failure via
  a test seam (`arm_subject_insert_failure`) and asserts the old slot stays live and `live_len` is
  unchanged. The resurrection case is likewise fixed: `unmark_deleted` now runs only after a
  successful commit.
- **Evidence:** `crates/vector-canister/src/facade/store/mutation.rs` (reorder + seam),
  `crates/vector-canister/src/facade/store/tests.rs`
  (`newer_stamp_upsert_commit_failure_keeps_old_slot_live`).
- **Related contracts:** [ADR 0032](adr/0032-vector-index-slab-page-store.md)

## Property and index capability gaps

The following property/index status was verified against the implementation at 2026-08-10 00:28:05 UTC +0000.
The canonical property-index contract remains [design/index/property-index.md](index/property-index.md);
this section records only the remaining gaps and the boundary at which each one is owned.
[ADR 0059](adr/0059-create-index-migration-backfill.md) is the accepted, partially implemented
source of truth for the migration-driven `CREATE INDEX` backfill lifecycle; this ledger does not
duplicate its state machine or ownership rules.

### Priority order

| Priority | Work item                                                                                  | Current status                                                                               |
| -------- | ------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| P0       | Backfill existing vertex, sidecar, and INLINE values before advertising an index as active | Closed — vertex/sidecar closed 2026-08-22 via GAP-2026-07-29-006; edge INLINE closed 2026-08-23 via GAP-2026-08-22-001 + GAP-2026-07-29-001 (identity rule + un-ignored cross-canister scenarios, 5/5) |
| P1       | Define INLINE removal/`NULL` transitions and complete vertex `MATCH` range planner wiring  | Closed 2026-08-22 — GAP-2026-07-29-004 resolved by contract-pinning tests (see entry); GAP-2026-07-29-002 closed 2026-08-21                                |
| P1       | Add edge range postings, Router seed planning, and execution support                       | Closed 2026-08-22 — GAP-2026-07-29-003 (see entry)                                           |
| P1       | Restore anchored multi-DML roll-forward saga convergence on both shards                    | Closed 2026-08-22 — GAP-2026-08-21-001 (see entry)                                           |
| P2       | Add vertex nested-record field indexes with a canonical dotted-path contract               | Open for slice-4 acceptance — GAP-2026-07-29-005 (ADR 0073 slices 1–3 implemented and validated; slice 4 landed on `39746f7b3`, acceptance pending Plan 0285 validation) |
| P2       | Add record/list index semantics and tests, after the scalar/leaf contract is fixed         | Planned                                                                                      |
| P2       | Extend edge-index anchors to accept `ScanValue::InList` union probes (symmetric with the vertex IN-list anchor landed 2026-08-24) | Resolved — GAP-2026-08-24-002 (`ScanValue::InList` reused end to end; planner fusion, three executor probe-union consumers, Router `EdgeEqualUnion` seeds) |
| P2       | Cypher bracket-form `IN […]` fails to parse under combined `cypher` + `sql-compat` builds (sql-compat arm requires `(` unconditionally) | Resolved — GAP-2026-08-24-003 (sql-compat arm claims only `(`-openers; per-combo feature matrix pinned) |
| P1       | Endpoint property projection on `EdgeBindEndpoints` replaces the vertex binding, so trailing `IsLabeled` filters drop every row of anchored edge scans with property-level projections | Resolved — GAP-2026-08-24-004 (planner entity-use veto; whole-variable projections were unaffected) |
| P2       | Extend edge-index anchors to accept `ScanValue::TextPrefix` prefix intervals (symmetric with the vertex STARTS WITH anchor landed 2026-08-24) | Resolved — GAP-2026-08-24-010 (edge symmetric extension landed 2026-08-24) |
| P3       | Decide edge-property uniqueness enforcement and multi-canister index sharding axes         | Planned                                                                                      |
| P3       | Cache resolved `GraphTypePropertySchema` beside the definition heap cache (steady-state O(1) catalog resolve; candidate from the 2026-08-25 canbench attribution review) | Open — GAP-2026-08-25-002 (catalog owner decision)                                           |
| P2       | ADR 0081 index-ordered ORDER BY delivery: Slice A (single-key ASC, equality/IN/range/prefix anchors, wire intent + executor tie-boundary TopK) landed; Slice B DESC and merge-aware cross-shard union deferred | Resolved 2026-08-25 — Slice A fix `7a7484eaf` + hardening `bench`/tests (cross-shard union tracked as GAP-2026-08-25-003)                                    |

The P0 item is a prerequisite for trusting any newly created index. The range premise is narrower:
the ordered scan primitive already exists through `StableBTreeMap::range()`; the remaining work is
to expose its capability at every required planner and entity boundary. Range support must not be
reimplemented as a full-bucket materialization.

### Range index status

Range traversal itself is **implemented**, not missing: `graph-index` converts the encoded bounds to
a half-open `StableBTreeMap::range()` scan, and paginated, label-sieved vertex range endpoints are
available. Router `SEARCH ... WHERE` also uses those endpoints for the supported numeric range
shapes. The missing pieces are planner coverage and edge symmetry, not the basic range scan.

### GAP-2026-07-29-001 — Existing INLINE values are not included by property-index backfill

- **Status:** Closed (2026-08-23). The Graph edge-property export enumerates both canonical
  value domains under one opaque cursor: sidecar `EDGE_PROPERTIES` rows first, then canonical
  edges carrying indexed inline values, decoded through the same `inline_index_values`
  membership resolution mutations use. The cursor carries a domain tag byte
  (`0x00` sidecar key, `0x01` inline `(wire label, owner)` position); untagged legacy cursors
  are rejected cleanly. Inline enumeration visits outgoing rows only and keeps the exact
  mutation identity — directed forward rows always, undirected max-endpoint rows only — so no
  mirror double-posting and every later Remove can address what backfill inserted. A vertex
  started within budget is collected fully (resume advances between vertices), so resume never
  skips or replays an identity; `done` is true only after both domains exhaust.
  ADR 0059's one-opaque-export consequence is now implemented as written. Owning unit tests:
  `backfill_enumerates_indexed_inline_scalar_values`,
  `backfill_emits_only_the_canonical_undirected_owner`,
  `backfill_resume_walks_both_domains_without_duplicate_identities`,
  `unversioned_backfill_cursor_is_rejected`. The deferred cross-canister create-index lifecycle
  proof (plans/0281 scenarios) initially exposed GAP-2026-08-22-001; after that identity fix
  landed (plan 0282), both un-ignored scenarios pass —
  `edge_inline_create_index_migration_converges_active_with_complete_postings` and
  `edge_inline_same_wasm_upgrade_mid_build_resumes_and_converges` in
  `crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, 5 passed / 0 failed at the
  closing run — completing this entry's migration-path validation over inline data, including
  equality/range completeness, per-shard coverage, multiplicity, undirected canonical-owner
  uniqueness, and same-wasm upgrade reopen.)
- **Severity:** P0 index correctness
- **Owner:** Router backfill orchestration and Graph inline-property backfill boundary
- **Observed behavior:** The edge-property backfill scans canonical `EDGE_PROPERTIES` only. Existing
  values stored in the edge inline-property region are therefore not enumerated by that backfill,
  although subsequent mutations can maintain an indexed inline property when the router supplies
  the active catalog.
- **Expected or needed behavior:** Creating or enabling an index on an eligible INLINE property must
  converge existing inline values before the index is advertised as active; the backfill cursor and
  posting owner must cover the inline storage domain as well as the sidecar domain.
- **Evidence:** `crates/graph/src/index/edge_property_backfill.rs` calls
  `scan_edge_properties_batch`; inline storage is owned by `crates/graph/src/edge_inline_property_schema.rs`
  and the edge store helpers. `design/storage/labeled-edge-inline-properties.md` describes the
  inline bytes as canonical for the declared inline property.
- **Impact:** A newly created index can miss pre-existing inline values and return incomplete
  equality/range candidates until those values are rewritten.
- **Next decision:** The [ADR 0059](adr/0059-create-index-migration-backfill.md) lifecycle (one
  Graph-owned opaque export, graph-index pull, touched-first exact outbox mutation, base-seed guard,
  and Active-only transition) is implemented with its cross-canister PocketIC proof complete
  (GAP-2026-07-29-006, closed 2026-08-22); this entry's remaining work is the edge `INLINE`
  enumeration itself. The existing operator cursor endpoints remain separate from that lifecycle.

### GAP-2026-07-29-002 — Normal `MATCH` planning does not select vertex range indexes

- **Status:** Closed — implemented (2026-08-21)
- **Severity:** P1 query capability
- **Owner:** Router catalog projection and GQL planner statistics boundary
- **Observed behavior:** The graph-index range API and `SEARCH` range path are present, but
  `RouterGraphStats::is_vertex_property_range_indexed` returned `false` unconditionally, so the
  normal `MATCH ... WHERE` planner could not use a vertex range index through its
  planner-statistics contract. Fixing only the stats gate surfaced the second half: the Router
  seed-anchor model had no range variant, so a leading non-Eq `IndexScan` reached graph shards
  with no index client and failed at runtime.
- **Implemented behavior:** Both halves are wired. The Router projects range capability from the
  same Active vertex catalog membership as equality (`planner_stats.rs`, fail-closed for
  unindexed properties); anchor selection lowers one-sided MATCH range predicates to
  `IndexScan` with the original predicate retained as a residual `PropertyFilter`; the Router
  seed model gained `IndexAnchor::Range` (`seed.rs::RangeSeedProbe` with `SeedRangeBound`)
  extracted from leading non-Eq `IndexScan` ops and resolved through the new
  `IndexLookup::lookup_range` paginated collector; Graph already skips any seeded leading scan op.
- **Evidence:** `crates/router/src/planner_stats.rs`, `crates/router/src/seed.rs`,
  `crates/router/src/index_lookup.rs`; planner contracts in
  `crates/gql-planner/tests/planner_tests.rs::match_range_anchor_*`; runtime contract
  `crates/pocket-ic-tests/tests/router_gql_query.rs::single_shard_vertex_index_match_range`.
- **Next decision:** None open for vertex MATCH ranges; two-sided bounds ride the same anchor via
  residual conjunction, and cross-type overscan clamping is a later performance refinement.

### GAP-2026-07-29-003 — Edge range index has no query/planner path

- **Status:** Closed — implemented (2026-08-22)
- **Severity:** P1 query capability
- **Owner:** Graph-index edge posting API, Router edge seed planning, and Graph edge candidate execution
- **Observed behavior:** Edge postings support equality lookup and paged equality lookup, but no
  `lookup_edge_range` endpoint, edge range request path, or edge range planner-statistics capability
  exists. The implemented `PostingRangeRequest` path is vertex-posting oriented.
- **Expected or needed behavior:** An edge property declared indexable should support the same
  encoded ordering contract for bounded range predicates, including label/direction scoping and
  resumable pages, before the planner emits an edge range scan.
- **Implemented behavior:** The ledger next-decision was adopted. graph-index gained the paginated
  ordered endpoint `lookup_edge_range_page`
  (`crates/graph-index/src/facade/store/edge_postings.rs`, key bounds derived in
  `crates/graph-index/src/posting_range.rs::edge_posting_key_half_open_range`) preserving the full
  `(physical_index_id, property_id, value, label_id, shard_id, owner_vertex_id, slot_index)` order
  with an in-canister wire-label sieve and the existing direction subset rule; kernel request type
  `LookupEdgeRangePageRequest` reuses `PostingRangeRequest` and the `EdgePostingCursor`. The Router
  projects edge range capability from the same Active edge catalog membership as equality
  (`RouterGraphStats::is_edge_property_range_indexed_for`, fail-closed), lowers one-sided leading
  range predicates to `IndexAnchor::EdgeRange(EdgeRangeSeedProbe)` and executes them drain+clamp:
  pages are drained within the 0270 comparison-domain interval (`gql::range_bounds_for_encoded_key`)
  and residual predicates stay as filters — no cursor state survives the call. The planner emits
  one-sided `PlanOp::EdgeIndexScan{cmp}` via the leading-edge fusion path (equality wins first),
  retaining the original predicate as a residual filter. The single-shard executor clamps through
  the shared helper and falls back to a canonical `EDGE_PROPERTIES` filtered superset scan for
  unsupported comparison domains.
- **Evidence:** storage contracts
  `crates/graph-index/src/facade/store/tests.rs::lookup_edge_range_page_*`; router contracts
  `crates/router/src/seed.rs::edge_range_scan_anchor_*`,
  `crates/router/src/gql.rs::edge_range_seed_probe_*`; planner contracts
  `crates/gql-planner/tests/planner_tests.rs::match_edge_range_anchor_*`; executor contract
  `crates/graph/src/plan/query/executor/scan/tests.rs::executes_edge_range_index_scan_with_domain_clamped_between`;
  runtime `crates/pocket-ic-tests/tests/router_gql_query.rs::single_shard_edge_index_match_range`
  and `federated_edge_index_match_range_with_domain_clamp`.
- **Next decision:** None open; `!=` complement policy remains Slice D.

### GAP-2026-07-29-004 — INLINE removal/NULL transitions need explicit posting semantics

- **Status:** Resolved (2026-08-22; contract pinned by owning tests — the implementation already
  satisfied the intended old-key/new-key semantics. Executor level: SET of top-level `NULL`
  rejects with `NullInlineProperty`, struct records missing a schema field or carrying a `NULL`
  leaf reject with `InvalidInlinePropertyValue`, and every rejection leaves the stored bytes
  untouched, so no stale posting can be produced. Store level: accepted updates dispatch exactly
  Remove(prev)+Insert(next) per membership, equal-value updates dispatch nothing, a leaf-scoped
  membership transitions only its own leaf key, and a width-mismatched update rejects before any
  posting or row change. Contract recorded in
  `design/storage/labeled-edge-inline-properties.md` §Mutation write semantics. Owning tests:
  `inline_edge_scalar_set_null_aborts_without_write`,
  `inline_edge_struct_set_missing_field_aborts_without_write`,
  `inline_edge_struct_set_null_leaf_aborts_without_write` (executor),
  `updating_indexed_inline_scalar_swaps_posting_old_for_new`,
  `updating_indexed_inline_scalar_to_same_value_emits_no_posting`,
  `updating_indexed_inline_struct_leaf_replaces_only_that_leaf_posting`,
  `updating_indexed_inline_bytes_with_wrong_width_rejects_before_write` (store). No code-path
  change, so no benchmark artifact update is required.)
- **Severity:** P1 index consistency
- **Owner:** Graph inline-property mutation and index posting dispatch
- **Observed behavior:** Inline index maintenance now supports eligible scalar values and struct leaf
  paths, but the contract for removing a field, assigning `NULL`, and changing a struct shape is not
  yet recorded as a complete old-key/new-key transition.
- **Expected or needed behavior:** Every mutation must remove the old sortable posting when the
  indexed value disappears or becomes non-indexable, and insert the new posting only after the
  inline bytes and decoded value agree. A missing/NULL value must never remain queryable as a stale
  posting.
- **Evidence:** `crates/graph/src/property/inline_dispatch.rs`,
  `crates/graph/src/facade/store/edge_profiles.rs`, and the inline-property contract in
  `design/storage/labeled-edge-inline-properties.md`.
- **Impact:** Deletes and shape changes can leave false-positive index candidates even when scalar
  replacement is correct.
- **Next decision:** Add mutation-contract tests for remove, NULL, missing nested field, and
  non-indexable replacement before widening inline index coverage.

### GAP-2026-07-29-005 — Vertex nested-record indexes are not yet symmetrical with edge INLINE fields

- **Status:** Resolved 2026-09-08 (slice-4 acceptance closed; uncommitted working-tree change, no commit per primary-owns-commits). Slices 1–3 are implemented and
  terminal-validated (focused unit gates, graph/graph-index all-target check/clippy, focused
  canbenches, posting-level PocketIC lifecycle, and an independent detached-worktree run at the
  `aa7fd9124` baseline plus only the remediation patch); domain decision recorded in
  [ADR 0073](adr/0073-vertex-nested-property-indexes-share-dotted-path-domain.md), 2026-08-22.
  Slices 1–3 landed: kernel `IndexedVertexMembership` gained required `field_path` +
  `ancestor_property_id` (Router-interned leaf identity plus the ancestor record property Graph
  dispatches by); Router DDL interns leaf and ancestor identities and validates bounded depth
  and typed leaf kind (`MAX_VERTEX_NESTED_INDEX_SEGMENTS`, container leaves rejected at declare
  time); vertex mutation dispatch, delete planning, the index-build fence, bulk writes, and the
  repair backfill all expand through one owner
  (`crates/graph/src/property/index_dispatch.rs::vertex_posting_transitions`) over the shared
  dotted-path resolver (`crates/graph/src/property/dotted_path.rs`), riding the existing
  old-key/new-key transition contract; migration builds export nested facts through
  `CanonicalExportTarget::Vertex.record_source`. Owning tests: router DDL interning/validation
  (`nested_vertex_ddl_*` in `index_catalog.rs`), GAP-004-grade store contracts for the record
  domain (`nested_record_*` in `catalog_context.rs`), record-walking backfill/export units, and
  the cross-canister lifecycle scenario
  `vertex_nested_create_index_migration_converges_active_with_complete_postings`
  (Building→Sealing→Active over pre-existing records on both shards including the three absence
  shapes, equality multiplicity, label scoping, and post-Active rewrite swaps).
  Slice 4 is present on main since `39746f7b3` (2026-08-23) but its acceptance stays pending
  Plan 0285 validation: the planner's property-access extractor lowers a nested chain to
  the canonical dotted path so equality/range anchors select interned leaf identities
  (`crates/gql-planner/src/anchor.rs::extract_property_access`, shared by equality/range/
  intersection anchor selection and inline-WHERE collection), the node-pattern value lookup uses
  the same extractor (`match_plan/path/filters.rs::find_equality_value_in_where`), and Router seed
  probes resolve dotted names through the ordinary catalog/namespace path. Plan 0284 removed the
  slice-4 capability test `planner_stats.rs::active_catalog_projects_nested_leaf_as_indexed_and_
  range_indexed`; Plan 0285 re-adds it and runs its bounded contracts, including planner tests
  `planner_tests.rs::match_nested_leaf_*`, router contracts
  `seed.rs::vertex_{equality,range}_anchor_resolves_nested_leaf_property`, and the
  cross-shard GQL proof
  `crates/pocket-ic-tests/tests/router_gql_query.rs::federated_vertex_nested_leaf_index_match_equality_and_range`
  — success itself is the anchor proof because unseeded leading index scans are rejected on graph
  shards. **Slice-4 acceptance closed 2026-09-08:** re-added `planner_stats.rs::active_catalog_projects_nested_leaf_as_indexed_and_range_indexed` (Active dotted-leaf projection for equality+range with ancestor/bare-segment/missing fail-closed plus catalog `field_path` membership assertion). Bounded contracts green: gql-planner `match_nested_leaf_*` + `match_unindexed_nested_leaf_range_does_not_emit_index_scan` (4 passed), router `seed::vertex_{equality,range}_anchor_resolves_nested_leaf_property` (2 passed), `planner_stats::` suite (7 passed), PocketIC `federated_vertex_nested_leaf_index_match_equality_and_range --exact` (1 passed / 24 filtered, 12.74s), router lib clippy clean, fmt + diff-check clean, pocket-ic `router_gql_query` target compiles.
- **Severity:** P2 query capability
- **Owner:** Planner property-path resolution; decision owned by ADR 0073 slice 4
- **Observed behavior:** Nested leaf postings were maintained and backfilled end-to-end, but the
  planner did not select a nested index as an anchor (`COST BY v.stats.field` parsed;
  equality/range planning over `n.stats.score` did not bind to the interned leaf namespace).
- **Expected or needed behavior:** The planner must seed anchors and range/equality candidates
  against interned leaf identities under the same contract as every other indexed property.
- **Evidence:** `crates/router/src/planner_stats.rs` (membership projection carries
  `field_path`/`ancestor_property_id`), `design/index/property-index.md` "Nested record leaf
  domain", and the ADR 0073 implementation slices.
- **Impact:** Vertex nested-field range/equality planning cannot rely on the same index contract
  as edge inline fields until the anchor lands.
- **Next decision:** Implement ADR 0073 slice 4 (planner anchors) in a follow-up plan.

### GAP-2026-07-29-006 — Index activation convergence gate lacks production driver/E2E completion

- **Status:** Closed 2026-08-22
- **Severity:** P0 index correctness
- **Owner:** Router index catalog and backfill lifecycle
- **Observed behavior:** Router now persists hidden Preparing/Building/Sealing/Aborting lifecycle
  rows and derives planner membership only from Active rows. Graph owns exact canonical export
  scopes, graph-index owns resumable build state, the production Router cross-canister driver
  composes register/advance/seal/cleanup with unit coverage, and the CLI exact-replays one immutable
  artifact. Graph label gain/loss admission is now implemented in the Graph coordinator: exact
  `(label_id, property_id)` memberships are selected from the canonical label set, all affected
  namespaces are preflighted before mutation, Building emits the exact build envelope, and Sealing
  rejects before the canonical label change. The boundary is covered by
  `building_label_gain_emits_exact_property_insert`,
  `building_label_loss_emits_exact_property_remove_once`,
  `sealing_label_gain_rejects_before_any_mutation`,
  `sealing_label_loss_rejects_before_any_mutation`,
  `public_mutation_id_zero_label_wrappers_preserve_index_build_admission`, and
  `delete_internal_label_clear_does_not_emit_second_property_removal`. The cross-canister PocketIC
  proof is now complete (`crates/pocket-ic-tests/tests/adr0059_index_build_lifecycle.rs`, three
  scenarios green at HEAD `3c75d2fb5`; inventory precondition `router_gql_query` 24 passed / 0
  failed): (a) one single-statement `CREATE INDEX` migration converges
  Preparing→Building→Sealing→Active over a two-shard federation with pre-existing vertex and edge
  sidecar data — equality/range reads stay correct throughout (label-scan fallback pre-Active,
  index-served post-Active, non-Person decoy excluded) and the router catalog projects
  Building→Sealing→Active in order; (b) while Building, an eligibility label gain is admitted and
  its pre-existing property value converges into postings, while Sealing an eligibility label
  loss rejects before canonical mutation (E2E mirror of the six Graph fence regressions); (c)
  upgrading all five federation canisters same-wasm mid-build preserves graph-index's registered
  build watermarks and resumes to Active with complete postings. Edge-INLINE backfill completeness
  stays owned by GAP-2026-07-29-001 and is deliberately not asserted. The "retryably fail-closed"
  posture resolved as documentation-only: no code-level gate exists — `apply_schema_migration`
  composes `real_index_migration_driver()` unconditionally and caller-driven exact replay (CLI
  `MAX_INDEX_APPLY_ROUNDS_PER_MIGRATION`) is the resumption contract.
- **Expected or needed behavior:** A newly declared index must remain pending until sidecar and
  INLINE backfill has completed for every attached shard, or the query planner must explicitly treat
  it as incomplete and fail closed. During Building, every label membership transition that changes
  eligibility for an already-indexed property must durably emit the exact per-physical-index build
  operation. During Sealing, that transition must reject before canonical state changes.
  Rebuild/drop must use the same lifecycle rule.
- **Evidence:** `crates/router/src/facade/store/backfill.rs`, `crates/router/src/planner_stats.rs`,
  `crates/graph/src/facade/store/labels.rs`, `crates/graph/src/index/catalog_context.rs`, and
  `design/index/property-index.md`'s router-owned active catalog description.
- **Impact:** A query can observe an index that is structurally present but incomplete, producing
  false negatives rather than merely falling back to a slower plan.
- **Confirmed implementation boundary:** Graph label mutation uses the ordinary label-pending path
  for label postings and the same index-build admission/fence owner as property transitions for
  exact label-scoped property eligibility. ADR 0059's touched-first BuildDml admission and
  pre-mutation Sealing rejection are covered by the six Graph regressions named above.
- **Resolution:** Catalog-epoch seal behavior and per-shard watermark convergence validated in
  PocketIC, including upgrade reopen; the migration endpoint's resumption contract is documented
  rather than gated. [ADR 0059](adr/0059-create-index-migration-backfill.md) remains the source of
  truth.
- **Follow-up observation (2026-09-11):** the executor-side guard for the same transient —
  `IndexScan` / `ConditionalIndexScan` / `IndexIntersection (no unique physical index
  namespace)` (`crates/graph/src/plan/query/executor/scan/index.rs:250,336,384`) — fails
  closed when more than one physical index is active for a property. Left as an observation
  under this closed entry; promote to its own GAP only if the transient is observed live.
  No wrong results (fail-closed); no error-path test exists.

### GAP-2026-07-29-007 — Edge uniqueness and index-canister sharding remain design work

- **Status:** Planned
- **Severity:** P3 product/capacity capability
- **Owner:** Router DDL/catalog for uniqueness; graph-index deployment and routing for sharding
- **Observed behavior:** Edge property indexes provide equality/range candidate postings but do not
  enforce uniqueness. Per-graph index clusters exist, while subject/range split axes across multiple
  index canisters remain planned.
- **Expected or needed behavior:** If uniqueness is exposed in DDL, the owning mutation boundary
  must reserve/check the key atomically with the canonical write. If an index is split, Router
  routing must preserve graph, subject, label, and range completeness without changing posting
  ordering semantics.
- **Evidence:** `design/index/property-index.md` Phase E and Non-goals; [ADR 0010](adr/0010-index-sharding-extensibility.md);
  current edge posting APIs in `crates/graph-index/src/facade/store/edge_postings.rs`.
- **Impact:** Declaring these capabilities prematurely would either allow duplicate values or make
  range/equality results incomplete across index canisters.
- **Next decision:** Keep both capabilities out of the public contract until their owner boundary,
  atomicity, routing, and rebuild semantics receive separate design decisions.

### GAP-2026-08-01-001 — gleaph-gql test harness: default-feature integration build broken, all-features suite hangs, clippy test lint fails

- **Status:** Resolved 2026-08-01 (commit pending; fixes in the same patch as the resolution)
- **Severity:** P3 test-harness
- **Owner:** `gleaph-gql` crate test harness
- **Observed behavior:**
  1. `cargo test -p gleaph-gql` (default features) fails to compile the `tests/parser_tests.rs`
     integration binary: `create_graph_type_nested_record_inline_property_ast` reads
     `PropertyDef.inline`, a `#[cfg(feature = "gleaph")]` field, without gating the test
     (parser_tests.rs L1378-1402).
  2. `cargo test -p gleaph-gql --all-features` does not terminate within the observation budget
     (10 minutes); `cargo test -p gleaph-gql --features cypher --lib` alone reproduces the hang,
     so the `cypher` feature's tests are the trigger.
  3. `cargo clippy -p gleaph-gql --all-targets --all-features -- -D warnings` fails on
     `clippy::items_after_test_module` in `type_check/phase_b.rs` (a `mod parameter_inference_tests`
     placed mid-file, followed by `pub fn infer_linear_query_binding_kinds`).
- **Root cause of the hang (2):** the deeply nested `VALUE { RETURN ...` input recursed through
  `Parser::recurse` until `MAX_RECURSION_DEPTH` (64), but the `cypher` build's enlarged
  expression/parse frames exhausted the native stack first — `EXC_BAD_ACCESS` (stack overflow) on
  the test worker thread, which the harness then waited on forever. Root cause confirmed by
  lowering the limit (guard fires, test passes) and by lldb (`EXC_BAD_ACCESS` at a stack address).
- **Resolution:**
  1. Gate `create_graph_type_nested_record_inline_property_ast` behind
     `#[cfg(feature = "gleaph")]` (the sibling `create_graph_type_edge_inline_property_ast` was
     already gated).
  2. Lower `Parser::MAX_RECURSION_DEPTH` from 64 to 32 with a doc note: the guard must fire at
     roughly half the heaviest feature build's stack cost; legitimate queries nest far below the
     bound. `cargo test -p gleaph-gql --all-features` now completes (all binaries pass, including
     the cypher-gated 746-test lib suite).
  3. Move `mod parameter_inference_tests` to the end of `type_check/phase_b.rs`;
     `clippy --all-targets --all-features -- -D warnings` now passes.
- **Evidence:** the three commands above before and after the fixes; `crates/gql/tests/parser_tests.rs`
  L1378-1402; `crates/gql/src/type_check/phase_b.rs`; `crates/gql/src/parser/helpers.rs`
  `MAX_RECURSION_DEPTH`; lldb backtrace showing `EXC_BAD_ACCESS (code=2)` at a stack address.
- **Impact (resolved):** default-feature test builds compile; the all-features suite terminates;
  test-target clippy gates with `-D warnings`.
- **Regression coverage:** the existing `deeply_nested_subqueries_are_rejected_not_overflowing`,
  `deeply_nested_parentheses_are_rejected_not_overflowing`, and
  `deeply_nested_not_chain_is_rejected_not_overflowing` tests run under both default and
  all-features builds and assert the depth error path.

### GAP-2026-08-25-001 — Pre-existing gleaph-gql parser failures: dashed prepared-query names fail `expect_ident` in EXECUTE publication forms

- **Status:** Resolved 2026-09-08 (same uncommitted fix as GAP-2026-08-24-005; discovered 2026-08-25 during plan 0304 / ADR 0080 implementation; not caused by that slice — reproduced on a pristine HEAD worktree).
- **Owner:** `crates/gql` tokenizer/identifier grammar and prepared-query naming contract.
- **Observed behavior:** `cargo test -p gleaph-gql --features gleaph --lib` fails
  `parse_grant_execute_on_prepared_query_to_public_and_principal` and
  `parse_revoke_execute_on_prepared_query_mirrors_grant`: parsing
  `GRANT EXECUTE ON PREPARED QUERY find-users TO PUBLIC` errors with
  `expected 'TO', got '-'`, i.e. the lexer splits dashed identifiers, so prepared names
  containing `-` cannot be granted/revoked through the publication form. All other grant
  tests pass; ADR 0080's new metadata forms are unaffected.
- **Impact:** Prepared operations with dashed canonical names cannot be published or revoked
  via GQL; the two failing tests document the intended contract (`find-users`), so the suite
  is red independent of any in-flight slice. Blocks a green `gleaph-gql` test run for every
  concurrent session.
- **Needed behavior:** Met 2026-09-08 — the `PREPARED QUERY` positions accept bare ADR 0061 kebab-case
  (`expect_prepared_query_name`) alongside the quoted form; no registration-side charset change.
- **Next decision:** None — both previously-red tests pass unmodified plus the new
  `parse_prepared_query_name_accepts_bare_kebab_digits_and_quoted` regression; same validation as
  GAP-2026-08-24-005 (567 gleaph / 524 default passed, fmt + diff-check clean).

### GAP-2026-08-25-002 — Catalog steady-state schema resolve rebuilds the full property schema on every call (heap-cache candidate)

- **Status:** Open — P3 optimization candidate recorded 2026-08-25 from the canbench
  re-baseline attribution review; catalog owner decision pending (not a defect: current
  behavior is correct, only redundant work)
- **Owner:** `crates/graph-catalog` (`GraphCatalog::try_property_schema_for_graph_id`,
  `lib.rs` ~L306) + callers that resolve per planning/validation
  (`router/src/facade/stable/graph_type_catalog.rs:352`)
- **Observed behavior:** Every call takes the `binding_cache` hit path but then performs
  `definition.clone()` plus `GraphTypePropertySchema::try_from_definition(&def)` — a full
  schema rebuild — before returning. Introduced by `8709ffe8c` (2026-07-30 heap-cache
  refactor); definitions themselves grew under `5396d15f3` (2026-07-29 inline edge
  properties). Net effect: `bench_catalog_resolve_inline_medium` roughly doubled against its
  stale artifact, 120,465 → 229,580 IC (+90.6%, absorbed into re-baseline `53e01eb26`).
- **Evidence:**
  - Profile ruled out experimentally: HEAD code with the pre-`d9a4f4b3e` codegen settings
    (`lto="off"`, `codegen-units=16`) in an isolated worktree measures 250.92K IC — +9.3%
    over the landed fat-LTO 229.58K. Fat LTO improves this bench; neither lto/cgu nor any
    2026-08-24/25 index slice contributes to the delta.
  - Artifact history: the prior value 120,465 IC was recorded at `72516cf84` (2026-06-19)
    and was never refreshed through the 07-29 → 08-03 graph-catalog evolution window
    (`5396d15f3`, `8709ffe8c`, `cee561df3`, `a7a361c66`).
  - Measurement hygiene for reproduction: isolated worktree checkout of HEAD; single-bench
    pattern run (`canbench resolve_inline` from `crates/graph-catalog`); no contact with
    other panes' uncommitted trees or shared-target contention.
- **Expected or needed behavior:** Cache the resolved `GraphTypePropertySchema` beside the
  definition/binding caches so steady-state resolves are O(1) map hits; invalidate on the
  same boundaries that evict the definition caches (`OR REPLACE`, migration apply,
  upgrade rebuild via `rebuild_caches_after_upgrade`).
- **Impact:** Steady-state read-path cost proportional to definition size paid on every
  planning/validation resolve; grows as graph types get richer (records, inline edges).

### GAP-2026-08-26-002 — No privileged authorization diagnosis surface; uniform Forbidden leaves "why denied" to trial-and-error

- **Status:** Resolved (2026-08-26). `EXPLAIN AUTHORIZATION FOR PREPARED QUERY` shipped per
  [ADR 0084](adr/0084-explain-authorization-diagnosis.md) (status: implemented): statement
  grammar behind the `gleaph` gate with zero-flag classification, Router report mode reusing
  `extract_live`/`requirements_cover` (stored-primary + live-drift fallback), visibility-only
  authority gates with indistinguishable `NotFound`, per-mode redaction, and the
  `adr0084_explain_authorization` PocketIC suite including an invariant-1 execution-bytes
  regression guard. Fixing commit: `cc80c5327` (suite) with grammar `7d6cf5083`, report mode
  `1ffce1888`.
- **Owner:** Router authorization seam (`crates/router/src/authz.rs`
  `enforce_data_plane_authorization`, requirement walker `walk_ops`/`requirements_cover`)
  together with the grammar surface in `crates/gql`; introspection precedent in
  `crates/router/src/gql_grants.rs` (`list_graph_grants`).
- **Observed behavior:** Every uncovered demand fails with the uniform non-disclosing
  `Forbidden` that never names the missing privilege or resource ([ADR 0074] §4). The only
  diagnosis paths today are reading code/tests plus owner-side `list_graph_grants`
  introspection; the question "why can't caller X run program P on graph G" has no direct
  answer anywhere in the system.
- **Expected or needed behavior:** A privileged-only diagnostic statement (working name
  `EXPLAIN AUTHORIZATION`, reserved by [ADR 0074] §4 and its Consequences trade-off) that
  renders one program's requirement set joined with coverage evaluation: per requirement,
  which effective row/root satisfies it or that it is uncovered — emitted only within the
  asker's visibility scope.
- **Evidence:** [ADR 0074] §4 ("diagnosis is a job for a future privileged-only
  `EXPLAIN AUTHORIZATION`") and §Consequences ("support/debugging of 'cannot see my own
  data' incidents requires either temporary explicit grants or the future
  `EXPLAIN AUTHORIZATION`"); reusable machinery: `RequirementSet` extraction and
  `requirements_cover` already exist as enforcement internals.
- **Impact:** Operability only — non-disclosure itself is intact by design. Developers and
  operators debug deny-by-default surprises via trial-and-error with temporary grants;
  shared-graph incident support stays script-level as multi-principal usage grows.
- **Next decision:** resolved by [ADR 0084] §1/§5: (a) self mode plus owner mode (`BY`)
  restricted to tenancy of every touched graph; (b) self-diagnosis may name uncovered
  resources on graphs whose visibility the asker passed — invisible graphs abort before any
  rendering; (c) full coverage detail with source-class redaction in self mode and full row
  identities in owner mode.

### GAP-2026-08-26-003 — main HEAD does not compile standalone: committed provisioning code imports `TextIndexId` defined only in uncommitted graph-kernel edits (half-commit recurrence)

- **Status:** Resolved (2026-08-26). Root cause: the text-index track landed referencing
  commits before their defining edits; fixed by landing the definitions as a
  single-purpose commit.
- **Resolution:** definitions landed in `31d716246`
  (`crates/graph-kernel/src/federation/shard_id.rs` +87 incl. `TextIndexId`,
  `federation.rs` re-export). Owner-side verification: check/clippy `-D warnings` /
  `test -p gleaph-graph-kernel --lib` 180/180. Coordinator independent verification:
  fresh detached worktree at `31d716246`, shared target dir,
  `cargo check -p gleaph-graph-kernel --lib` → `Finished dev profile … 15.41s`, zero
  errors/warnings — HEAD builds standalone again.
- **Owner:** the text-index/canister track session holding the dirty defining edits
  (`crates/graph-kernel/src/federation.rs`, `crates/graph-kernel/src/federation/shard_id.rs`
  — `TextIndexId` at `shard_id.rs:246` with its `federation.rs:54` re-export). The
  referencing side landed first: committed
  `crates/graph-kernel/src/provisioning/{mod.rs,tests.rs}` import the symbol via
  `crate::federation::{… TextIndexId …}`.
- **Observed behavior:** a clean detached worktree at HEAD fails
  `cargo check -p gleaph-graph-kernel --lib`:
  `error[E0432]: unresolved import crate::federation::TextIndexId` at
  `provisioning/mod.rs:12:50` ("no TextIndexId in federation"). Main-tree
  `gleaph-router --lib` test targets additionally fail on text-index/index-migration WIP
  per the w1:p8 report.
- **Expected or needed behavior:** every commit leaves main buildable standalone: a fresh
  clone/worktree compiles kernel lib and router lib tests without any other session's
  uncommitted state.
- **Evidence:** coordinator reproduction (2026-08-26): `git worktree add … --detach HEAD;
  CARGO_TARGET_DIR=<shared> cargo check -p gleaph-graph-kernel --lib` → E0432 above;
  w1:p8 working log; `plans/0307-group-element-id.md` Validation Transcript. Note HEAD
  advanced during verification (`35c18f85c` → `e24bb57c5` via text-track commits
  `c97dc08c5`, `23b23249c`) and the breakage reproduces on current HEAD — live incident,
  not history.
- **Impact:** bisect false positives across the whole workspace; fresh clone/worktree
  cannot compile kernel or run router tests; any per-commit validation gate is blind.
- **Next decision:** the owning text-track pane lands its graph-kernel defining edits
  immediately (simultaneous-landing resolution), or a coordinator-approved minimal
  extraction commit moves the definitions in; afterwards re-run the clean-worktree check
  to close this entry.

### GAP-2026-08-26-004 — ANY-SHORTEST-style prepared ops are undispatchable: label-only multi-variable anchor prefixes dispatch without seeds and the federated wire guard rejects them

- **Status:** Resolved (2026-08-26). The original index-servability framing (plan 0309
  draft, briefs #5/#6) is superseded by the root cause below; both halves are fixed and
  pinned.
- **Owner:** Router seeding (`crates/router/src/seed.rs` `SeedAnchorSet::from_plans`, the
  ADR 0046 Phase 1 label-only fallback) against the graph wire guard
  (`crates/graph/src/plan_wire_guard.rs::ensure_federated_seeds_for_index_anchors`); the
  apply-time index-anchor half lives in `crates/router/src/prepared.rs`
  (`validate_leading_index_anchor_servability`). gql/gql-planner untouched throughout.
- **Observed behavior:** plans planned with ≥2 leading labeled endpoint scans —
  structurally every `ANY SHORTEST` between two labeled endpoints — were rejected at run
  time for every caller on federation-configured shards with
  `unsupported plan query operator: IndexScan(no index client)`: the label-only
  multi-variable fallback dispatched them seed-less while the guard requires effective
  seeds exactly for labeled-scan-led plans. First recorded as the knowledge-demo
  `shortest-path` failure (0296 bring-up evidence; 2026-08-24 matrix row), mis-framed
  through plans 0309/0310 as an index-catalog mismatch until the PocketIC probe in 0310
  falsified that reading.
- **Expected or needed behavior:** label-only multi-variable prefixes must be seedable on
  the topology that can serve them (standalone), stay fail-closed where they cannot
  (multi-shard), and unservable index anchors must fail at apply time instead of run time.
- **Resolution:** (a) `SeedAnchorSet::from_plans` keeps pure label-only multi-variable
  prefixes on single-live-shard topologies; anchors resolve through the existing
  label-posting lookup (`lookup_label`, ADR 0004) and the sole shard executes remaining
  labeled scans locally. Multi-shard keeps fail-closed `None` pending the
  federated-traversal restoration ADR ([federation-target.md](sharding/federation-target.md)).
  (b) prepared apply rejects leading index anchors without a unique Active posting
  namespace (migration-000002 hazard class). Fixing commits: standalone seeding + E2E
  (`feat(router): seed label-only anchors in standalone topology`) and the apply-time
  guard (`feat(router): guard prepared apply against unservable index anchors`),
  2026-08-26.
- **Pinned contracts:** `with_hits_encodes_seed_bindings_blob_for_single_shard_dispatch`,
  `from_plans_keeps_multi_variable_label_only_prefix_on_standalone`,
  `from_plans_multi_shard_still_drops_label_only_multi_variable_prefix`,
  `wave_4_multi_anchor_seed_extracts_two_variables` (router);
  `shortest_path_demo_plan_leading_ops_diagnostic_dump` (router, plan 0309);
  `index_scan_requires_index_client` (graph); PocketIC
  `knowledge_demo_shortest_path_flow.rs` (default-deny then non-owner row-content
  execution after publication).
- **Next decision:** none open. Multi-shard label-only-multi seeding returns with the
  federated-traversal restoration ADR.

### GAP-2026-08-28-002 — HEAD does not compile `gleaph-cli` standalone: committed `prepared.rs` matches `ReturnBody::NoBindings` (a `cfg(cypher)` variant) while the `cli/Cargo.toml` cypher-feature line is uncommitted (half-commit recurrence)

- **Status:** Open (2026-08-28) — same class as GAP-2026-08-26-003, different crate pair.
- **Severity:** P1 implementation-integrity recurrence
- **Owner:** cli stream (the pane holding the uncommitted `crates/cli/Cargo.toml` +
  `crates/cli/src/identity.rs` dependency-migration work)
- **Observed behavior:** A clean worktree at HEAD (2026-08-28, post `00c94208e`) fails to
  compile `gleaph-cli`: `crates/cli/src/prepared.rs:1077` matches
  `ReturnBody::NoBindings`, which exists only under gleaph-gql's `cfg(feature = "cypher")`,
  and the committed `cli/Cargo.toml` declares `gleaph-gql` features `["gleaph", "serde"]`.
  The main working tree compiles only because the stream's uncommitted Cargo.toml WIP adds
  the `cypher` feature (plus a `gleaph-gql-planner` cypher feature and the k256/rand
  dependency migration). Isolated-worktree probes (e.g. `git worktree add` for a baseline
  build) therefore cannot build the cli at HEAD.
- **Expected or needed behavior:** the cli stream lands its Cargo.toml feature lines in the
  same commit window as any prepared.rs AST matching; until then, per-commit validation
  gates on clean checkouts stay blind for the cli crate.
- **Next decision:** the owning stream commits the feature lines (the workspace comment in
  the WIP already explains why `cypher` must stay enabled unconditionally); afterwards a
  clean-worktree build check closes this entry.

### GAP-2026-08-26-005 — Real Provision upgrades wipe the bootstrap authority and active release: eager `StableCell::new` constructors clobber durable cells on every process restart

- **Status:** Resolved (2026-08-26, slice "Provision upgrade durability"). The two eager
  constructors now use the read-or-create form `StableCell::init(memory, default)`
  (`crates/provision/src/stable/bootstrap_auth.rs` `init_bootstrap_auth_cell`, MemoryId 4;
  `crates/provision/src/stable/memory.rs` `init_active_release`, MemoryId 10), matching the safe
  precedent of `init_artifact_storage_id`. `init` decodes an existing durable cell and only writes
  the default when the region is empty (fresh install), so a process restart no longer rewrites
  state before any handler runs. No other store logic changed; `post_upgrade` remains a no-op.
- **Owner:** `crates/provision/src/stable/bootstrap_auth.rs:26-38` (`BOOTSTRAP_AUTH`
  thread-local, MemoryId 4) and `crates/provision/src/stable/memory.rs:111-116`
  (`init_active_release`, MemoryId 10).
- **Observed behavior:** after a real chunked upgrade of a deployed Provision (PocketIC
  replica ingress through the `gleaph-operator` transport), governance readback that
  succeeded pre-upgrade returns `Unauthorized` because
  `ProvisionBootstrapAuthStore::get_authority()` reads `None`. Stable memory is preserved
  byte-for-byte across the upgrade (25,231,360 bytes unchanged, measured via canister
  status), so the loss happens in the store layer, not the replica.
- **Root cause:** `StableCell::new(memory, value)` writes on construction
  (`ic-stable-structures` cell.rs:174-176). Both cells are built inside eager
  thread_local initializers with `None` defaults, so every new process (install or
  upgrade) rewrites the durable cells empty before any handler runs. The same module set
  already uses the safe read-or-create form elsewhere
  (`init_artifact_storage_id`, `stable/memory.rs:100-105`), and the hazard is described in
  `bootstrap_auth.rs:58-60`'s own comment. The unit test
  `bootstrap_authority_singleton_survives_upgrade` passes because it never restarts the
  process, so the writing constructors never run again.
- **Expected or needed behavior:** an upgraded Provision must retain its seeded authority
  and active release without re-running `init`; a fresh install must still start empty.
  Blast radius if unfixed: a post-upgrade Provision rejects every catalog write as
  unauthorized and aborts every issuance at the no-active-release guard while remaining
  live.
- **Pinned tests:** unit — `stable::store::tests::bootstrap_auth_constructor_rerun_preserves_seeded_authority`
  and `active_release_constructor_rerun_preserves_active_release` rebuild each cell through its
  constructor over the same MemoryId (process-restart simulation via new test-only reopen helpers);
  both were red-proofed against the write-on-build form. E2E —
  `adr0087_bootstrap_tier::bootstrap_tier_deploys_and_upgrades_provision_end_to_end` now publishes
  five placeholder artifacts plus a release before upgrading and asserts post-upgrade: governance
  `artifact_audit_history` authorized with the pre-upgrade successful activation row intact,
  anonymous still unauthorized, and `release_get_active` equal to the pre-upgrade release id.

### GAP-2026-08-29-001 — RESOLVED: `ELEMENT_ID(e)` / `GraphPathEdgeId` bytes collide across edge labels under the same owner (router/storage edge-identity bug)

- **Status:** Resolved (plan 0312 / [ADR 0090](adr/0090-edge-element-id-label-attribution.md),
  2026-08-29). Identity fix; no data loss.
- **Severity:** P2 explorer display / cross-caller identity collision; not data loss.
- **Owner:** `crates/graph-kernel/src/federation/{global_edge_id,encoded}.rs` and
  `crates/graph-kernel/src/path.rs` (canonical identity + Feistel encoder/decoder);
  `crates/graph/src/plan/query/executor/path/materialize.rs` and `eval.rs` (call sites).
- **Observed behavior:** Knowledge demo seed has 37 unique edges. `MATCH (a)-[e:L]->(b)
  RETURN element_id(e)` returned 25 unique element_ids, not 37. The native graph explorer
  dedups by element_id and rendered 25 edges, missing 12. No data loss: storage is
  correct; the wire id is not. The collision is masked in graphs where every owner has at
  most one label (e.g. simple seeds), so the bug only surfaced against a multi-label
  source vertex (the demo's alice has `AUTHORED_BY slot0`, `BELONGS_TO slot0`, `OWNS slot0`,
  `ROUTED_VIA slot0` — all four encode to the same `(shard, alice, 0)`).
- **Expected or needed behavior:** `ELEMENT_ID(e)` / `PathElement::Edge` bytes must be
  unique per edge under the per-graph `ElementIdEncodingKey`. The CLI stderr warning
  (introduced 2026-08-25) already states that edge ids are unstable physical handles
  unstable across compaction; this bug is the same kind of instability in a different
  dimension (cross-label collision at the same compaction), so the warning covers the
  fix surface; no new warning is needed.
- **Resolution:** `GlobalEdgeId` extended from `(shard, owner, slot)` to
  `(shard, owner, label, slot)` with the label widened from `u16` to `u32` for 4-byte
  alignment and bijection. `EncodedEdgeId` wire layout grew from 12 to 16 bytes. The
  Feistel encoder/decoder is updated to a 16-byte canonical form (head: 8-byte Feistel-4,
  tail: 8-byte XOR under a key-derived mask that mixes the encoded head and the per-graph
  key tail). `GraphPathEdgeId::new` is now 5-arg and takes the label; the 3-arg form is
  removed. Caller sites in `materialize.rs` (encoder + path materialization) and `eval.rs`
  (singleton + `EdgeGroup` + host test helper) read the label from `EdgeHandle.label_id`
  (already populated at insert time in `edge_insert.rs:203,331`). A new pure helper
  `catalog_label_from_lara` translates the LARA bucket label to the catalog label space
  for the wire encoder. The remote-edge default-label `0` matches the existing
  label-free reverse-edge pattern.
- **Pinned tests:** host — `gleaph-graph` lib tests
  `edge_element_id_distinguishes_per_label_same_owner_slot`,
  `edge_element_id_distinguishes_within_same_label`,
  `edge_element_id_distinguishes_per_owner_same_label_slot`,
  `edge_element_id_encoded_length_is_16_bytes`,
  `edge_element_id_encoded_differs_from_canonical`,
  `edge_element_id_label_zero_is_distinct_from_label_nonzero`;
  `gleaph-graph-kernel` lib tests `global_edge_id::round_trip`,
  `global_edge_id::label_widening_is_lossless_for_catalog_range`,
  `encoded::edge_encode_decode_roundtrip`,
  `encoded::edge_tail_changes_when_head_unchanged`,
  `path::edge_path_id_roundtrips`,
  `path::edge_path_id_distinguishes_per_label`. The plan 0307 group-binding
  executor tests (`element_id_on_edge_group_lists_hop_ids_in_traversal_order`,
  `element_id_on_vertex_group_preserves_order_and_empty_group_yields_empty_list`,
  `element_id_on_path_group_stays_fail_closed_with_guidance`) still pass; the per-hop
  list form inherits the fix automatically because the singleton and list arms share
  the encoder.
- **Evidence:** detection run during knowledge demo validation against the native graph
  explorer (2026-08-29); root cause read of
  `crates/graph/src/plan/query/executor/path/materialize.rs:116,145`,
  `crates/graph/src/plan/query/executor/eval.rs:1010,1024,2214`,
  `crates/graph-kernel/src/federation/global_edge_id.rs`,
  `crates/graph-kernel/src/federation/encoded.rs`. Post-fix validation: `cargo test -p
  gleaph-graph --lib` (1124 passed, 0 failed), `cargo test -p gleaph-graph-kernel --lib`
  (184 passed, 0 failed), `cargo clippy -p gleaph-graph --lib --all-targets -- -D
  warnings` and the same for `gleaph-graph-kernel` (exit 0).
- **Related contracts:** [ADR 0090](adr/0090-edge-element-id-label-attribution.md) (new);
  [ADR 0005](adr/0005-vertex-identity.md) (amended — 12 → 16 bytes for `GlobalEdgeId` /
  `EncodedEdgeId`, 12-byte form marked SUPERSEDED); [ADR 0006](adr/0006-pre-federation-foundation.md)
  § 4 (amended); [ADR 0019](adr/0019-graph-local-shard-id-and-index-clusters.md) § 3
  (amended); [glossary](glossary.md) rows for `Global edge id` and `Encoded edge id`
  (amended); [group-variables](execution/group-variables.md) ELEMENT_ID rules row
  (amended with cross-link to ADR 0090). Distinct from GAP-2026-08-26-001 (plan 0307,
  group-binding executor implementation): the 0307 fix made `ELEMENT_ID(e)` over a
  quantified-path group variable execute, but the underlying identity collision was not
  in scope and is resolved here.

### GAP-2026-08-29-007 — RESOLVED: Canonical mutation segment enforcement was not path-independent

- **Status:** Resolved (2026-08-29; implementation slice landed; status updated alongside the
  slice commit)
- **Severity:** P1 latent invariant enforcement gap
- **Owner:** Graph (`crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`)
- **Observed behavior:** The canonical mutation segment relied on two narrow structural
  properties to keep inter-canister calls out of the segment: (a) it takes no
  `PropertyIndexLookup` handle, and (b) the only `CALL` procedures it executes are
  synchronous. These are not path-independent — a second inter-canister chokepoint (peer-shard
  client, subgraph client, Phase 6 cross-shard coordination) added inside the segment would
  silently extend the critical section across a commit point. ADR 0029 §8 explicitly named this
  as a deferred guard.
- **Expected or needed behavior:** A new inter-canister chokepoint added inside the segment must
  fail loudly at its acquisition boundary. The canonical segment must trap and roll back the
  whole message (Property 5) when a chokepoint is reached from inside the segment.
- **Resolution (design):** Adopted [`CanonicalSegmentGuard`](../adr/0091-path-independent-canonical-segment-guard.md)
  (RAII thread-local depth counter + Drop balance check + chokepoint-side
  `assert_no_canonical_segment(...)` trap). The guard is entered at the first statement of
  `apply_canonical_mutation_segment`; the existing `ExecuteCtx::new` (referred to as
  `ExecutorContext::new` in ADR 0091) chokepoint for `PropertyIndexLookup` acquisition calls
  `assert_no_canonical_segment("executor_context_new")`. The original "no `PropertyIndexLookup`
  handle" / "synchronous `CALL`" guarantees are retained as defense-in-depth.
- **Resolution (implementation):** Landed. New module
  `crates/graph/src/facade/canonical_segment.rs` (thread-local `Cell<u32>` depth counter,
  `CanonicalSegmentGuard::enter()`, `Drop`-balance trap, `assert_no_canonical_segment(...)`,
  `canonical_segment_depth()` accessor). One-line `enter()` at the first statement of
  `apply_canonical_mutation_segment` (`crates/graph/src/gql_run.rs`). One-line
  `assert_no_canonical_segment("executor_context_new")` at the top of
  `ExecuteCtx::new` (`crates/graph/src/plan/query/executor/context.rs`). Re-exports in
  `crates/graph/src/facade.rs`. Test-only seam (`pocket-ic-e2e` only) in `canonical_segment.rs`
  plus a feature-gated `GLEAPH.E2E_SIMULATE_INTER_CANISTER_CALL` CALL procedure dispatch in
  `crates/graph/src/plan/mutation/gleaph_finalize.rs`.
- **Evidence (post-implementation):** New host unit tests in
  `crates/graph/src/facade/canonical_segment.rs` cover depth-balance, outside-guard pass,
  inside-guard trap, nested-enter depth, and a contrived `Drop`-balance wrong-impl test
  (`double_drop_traps_when_depth_already_zero`). New PocketIC test file
  `crates/pocket-ic-tests/tests/adr0091_path_independent_guard.rs` covers whole-message
  rollback when the trap fires, the happy path, and the read-side chokepoint pass. Existing
  tests are unaffected (`canonical_segment_trap_rolls_back_whole_message` passes without
  modification because Property 5 guarantees message-wide rollback on trap).
- **Impact:** The canonical segment atomicity boundary is now formally defensible from any new
  inter-canister chokepoint by a single line of defense at the chokepoint boundary, ahead of
  the second-chokepoint trigger named in ADR 0029 §8. ADR 0091 Decision 4 (PR-review
  checklist) remains the only manual guard for future drift; the runtime guard is the
  single-line `assert_no_canonical_segment(...)` at the chokepoint acquisition boundary.
- **Related contracts:** [ADR 0091](../adr/0091-path-independent-canonical-segment-guard.md)
  (new); [ADR 0029 §8](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md)
  (referenced from ADR 0091); [ACID roadmap Phase 1](../architecture/acid-roadmap.md) exit
  criteria (updated to cite ADR 0091); [`MutationToken` /
  `ReadMode::AtLeast` doc comments](../../crates/graph-kernel/src/plan_exec.rs) (clarified
  scope; see `gleaph-mvcc-and-ic-atomicity.md` and `gleaph-mvcc-design-review.md` for the
  underlying review).

## Review cadence

- The primary agent checks this ledger before final approval of a meaningful slice.
- A slice that resolves an entry updates its status in the same commit as the fix.
- Open entries should be converted to an implementation plan when their prerequisite arrives or
  when they become the highest-impact blocker.
- If an entry duplicates an existing roadmap or ADR item, replace its detailed proposal with a link
  to that authoritative contract rather than maintaining both descriptions.

### GAP-2026-09-02-003 — RESOLVED: LPB-in-tree split-brain layout (Plan 0326 REWORK)

- **Status:** Resolved (2026-09-02, Plan 0326 REWORK).
- **Severity:** P0 split-brain — read path and write path disagreed on
  the property root location. Production correctness + benchmark
  measurement validity.
- **Owner:** `promote_bypass_to_tree_mode` (promote.rs:300-465), 
  `tree_mode_property_leaf_append` (tree_write.rs:421-567), depth-1
  tail append (tree_write.rs:611-794), depth-2 tail append
  (tree_write.rs:856-1196), `tree_mode_demote_to_slab` Phase 5e
  (tree_write.rs:2260-2300), `bucket_span_region_len` and
  `property_root_region_len` (compact.rs:234-300).
- **Observed behavior:** the Plan 0326 first cycle shipped with a
  read path that derived the property root as `edge_start +
  bucket_span_region_len(bucket)` (contiguous layout, ADR §2) and a
  write path that allocated the property root as a separate LEG
  span, recorded in the descriptor's `inline_property_bytes_offset`
  field. The two layouts agreed only when the allocator happened to
  place the property root span adjacent to the edge root span.
  Synthetic bench seeds (in `seed_w32_sweep_bucket`) constructed
  both spans manually, so the split was hidden. The canbench
  reported 540 ins for both new benches (4K and 65K), which was
  actually a no-op: the bench seed left the vertex's `bucket_count`
  at 0, so `find_bucket` returned `BucketSearch::Missing` and the
  visit callback was never invoked. The unit test
  `lpb_in_tree_demote_round_trip_at_w_4_stored_4096` passed only
  because the synthetic seed's property root span happened to
  land at `edge_start + edge_root_len` by allocator luck.
- **Why it matters:** the actual production deployment would have
  hit the read-after-write inconsistency on the first w>0 tree-mode
  bucket promote: property reads via the production visit path
  would return bytes from the wrong LPB blocks. The bench numbers
  reported in the yml were 0% noise; downstream regression budgets
  (Plan 0324's Gate 3) compared against these bogus values.
- **Fix (REWORK)**: single combined LEG span `[edge root | property root]`
  (gap 0, ADR §2). All write sites (promote Phase 2, depth-1 tail
  append, depth-2 tail append, depth-2 new-interior-mint) do a
  single `allocate_span_avoiding(combined_len, avoid=old_combined_span)`
  and copy both halves. The descriptor's `inline_property_bytes_offset`
  is set to 0 in tree mode (unused; the property root location is
  derived from `edge_start + bucket_span_region_len(bucket)`). The
  read path is unchanged (was already correct per ADR §2). The
  demote's Phase 5e reads the property root from `edge_start +
  bucket_span_region_len(bucket)` and releases the property half of
  the combined span (the edge half is covered by the edge-half
  release in Phase 5c). The compact helper `property_root_region_len`
  returns `ceil(S / K)` (no tail buffer; the legacy `+ 1` was a
  writing-side convenience that never affected the read path). A new
  compact helper `combined_span_region_len` returns
  `edge_root_len + property_root_len` for the compaction rewrite
  path's per-vertex span intervals.
- **F-2 cap guard**: a new `LabeledOperationError::PropertyTreeRootCapacityReached`
  variant fires when the property root would exceed `R_MAX = 1024`.
  Property-tree deepen (with `BlockKind::InlinePropertyInterior`) is
  recorded as a follow-up slice per ADR 0088 §7.
- **F-3 bench honesty**: `tcsr_4096_property_read_w32` and
  `tcsr_65536_property_read_w32` now use the production insert path
  (`insert_edge_skip_leaf_cascade`) with the bucket schema pre-declared
  via `ensure_label_bucket_inline_property_byte_width`. The two bogus
  540-ins entries (which measured nothing) are removed from
  `canbench_results.yml`. New honest measurements: 4K = 3,160,000 ins,
  65K = 5,630,000 ins. `--persist` is forbidden going forward
  (single-bench `canbench <name>` runs only; yml updates are manual).
- **F-4 w=0 hot path**: the depth-2 interior-row-append path for w=0
  no longer reallocates the edge root (the edge root doesn't grow
  on interior-row append; only on new-interior-mint does the edge
  root grow). The depth-1 / depth-2 combined-span allocation uses
  `allocate_span` (no avoid) for the w=0 path to avoid the avoid-scan
  cost. The +24.81% regression on `tcsr_1048576_deepen_beyond_r_max`
  is fully restored to 0%.
- **Tests**: 630 lib tests pass (+4 from 627: 
  `lpb_in_tree_rework_combined_span_round_trip` exercises the full
  promote → read round-trip via the production path; 
  `lpb_in_tree_rework_f2_cap_guard` exercises the synthetic
  R_MAX-1 setup; plus 2 from the foundation). 583 canbench tests pass.
  clippy -D warnings clean. fmt clean. validator final phase: PASS.
- **F-5 wasm chars**: canbench wasm builds and runs; exported name
  count within the 20,000 char limit. Baseline 15,684 → 15,684 (no
  new bench surface). 159 → 161 entries (+2 honest benches; the
  partial slice's bogus 540-ins entries were removed).

### GAP-2026-09-07-001 — RESOLVED: Delete-only workloads accumulate tombstones indefinitely on Insertion-policy slab buckets and default-label bypass rows

- **Status:** Resolved (2026-09-07, Plan 0339 slab side + Plan 0341 bypass side). The
  slab-bucket half is closed by the Plan 0339 remove-side hysteresis admission trigger
  (`DeferredBidirectionalLabeledLaraGraph::maybe_enqueue_remove_side_compaction`, deferred.rs):
  after a successful removal, if post-removal tombstones exceed half the stored width on an
  Insertion-policy slab bucket, the existing `CompactVertexEdgeSpanV1` work item is enqueued and
  the same message's drain left-packs the span. Regression tests: `remove_side_compaction_*`
  (deferred.rs) and `delete_past_hysteresis_enqueues_span_compaction_and_left_packs` /
  `delete_below_hysteresis_leaves_tombstones_for_append` (facade/store/tests.rs). The bypass-row
  half is closed by Plan 0341: the bypass-origin geometry contract
  (`design/storage/lara.md`) plus the new `compact_default_bypass_row` left-pack step
  (compact.rs), the `CompactDefaultBypassRowV1` work item, and the remove-side admission wired
  into the deferred wrapper's bypass remove paths (deferred.rs). Regression tests:
  `bypass_tombstones_are_left_packed_at_hysteresis` / `bypass_compaction_below_hysteresis_no_fire`
  (deferred.rs) and `bypass_compact_step_*` (bypass.rs). The slab trigger + this step together
  close the full gap.
- **Severity:** P2 maintenance-coverage gap (slow scan/memory degradation, no correctness risk;
  extent-bounded per row)
- **Owner:** `ic-stable-lara` deferred maintenance admission (`deferred.rs`) + bypass row
  compaction coverage (`compact.rs` / `bypass.rs`)
- **Observed behavior:** The insert path admits post-write maintenance — dense-leaf compaction
  when `labeled_leaf_segment_is_dense(src)` and inline-property-bytes slab compaction
  (`deferred.rs:2172-2194`, admission failures trap). The remove path admits nothing: production
  `remove.rs` contains no enqueue/mark_compact/maintenance call, and the sole remove-side trigger
  is the tree-mode demote check (`degree <= T_DEMOTE`, Plan 0319, best-effort inline). A
  delete-only workload therefore accumulates tombstones indefinitely on Insertion-policy slab
  buckets and default-label bypass rows until a later insert touches the same vertex.
  `Unordered` labels are excluded from the gap by policy: they self-heal on insert (in-slab
  tombstone reuse before append, `insert.rs:420`) and swap-compact (ADR 0052 §7).
- **Plan 0339 survey findings (2026-09-07, no code changed):**
  1. **Insertion-policy slab buckets** are closable with an existing work item:
     `CompactVertexEdgeSpanV1` (enqueued via `mark_compact_vertex_edge_span`, deferred.rs:1542)
     drains through `compact_vertex_edge_span_one_step` (compact.rs:1923), which left-packs a
     tombstoned bucket span. The planned trigger design (hysteresis
     `tombstones = stored − degree ≥ stored/2` post-removal, Insertion-only policy gate via
     `maintenance_policy_for_label`, tree/Unordered exclusions, trap-on-admission-failure per the
     insert-side convention, O(1) descriptor-arithmetic gate) is implementable as written for
     this regime.
  2. **Default-label bypass rows are NOT closable today**: every existing edge-span compaction
     path short-circuits on `is_default_edge_labeled()` (`compact_vertex_edge_span_one_step`
     compact.rs:1939, `rewrite_vertex_edge_span` compact.rs:739, leaf-span helpers), and
     `CompactVertexValueSpanV1` drains as a no-op. Enqueuing any existing work item for a bypass
     row is a drain no-op. The structural reason: bypass rows are vertex-level spans whose
     left-pack would interact with the bypass-origin geometry (`base_slot_start` updates of
     later tail rows, `bump_successor_origins_after_bypass_end`) — a real compaction capability
     that does not exist, not a missing enqueue.
  3. The planned test update (`bypass_accumulates_many_slab_tombstones_without_promotion`,
     bypass.rs:617) exercises the unidirectional `LabeledLaraGraph` remove path, not the
     deferred-wrapper remove side where the trigger would live — the test relocation/rename is
     part of the bypass work, not the trigger work.
- **Expected or needed behavior:** (a) a remove-side hysteresis admission trigger for
  Insertion-policy slab buckets (design fixed in research doc §5.5 and validated by the 0339
  survey); (b) a bypass-row left-pack compaction capability (new compact step or remove-path
  maintenance in `compact.rs`/`remove.rs`/`bypass.rs`, respecting the bypass-origin geometry and
  the ADR 0022 recorded descending-fallback behavior), with the bypass regression test moved to
  the layer that owns the trigger.
- **Impact:** Delete-only workloads degrade scan cost and pin memory on affected rows
  (extent-bounded; the dense fast path re-engages once compaction eventually fires via a later
  insert). No correctness risk; the 0337 slab rule (slab OFFSET overshoot ~109K at the legal
  cap) remains deferred-with-evidence, and this trigger is the recorded lever that would also
  reclaim those tombstones.
- **Next decision:** Split delivery — (1) a small plan for the slab-bucket trigger
  (`CompactVertexEdgeSpanV1`, design already fixed), then (2) a separate bypass-row compaction
  plan (new compact step; needs the bypass-origin geometry contract written down first).
  Update this entry to Resolved in the same patches as each half lands.
  **Both halves landed (2026-09-07):** Plan 0339 (slab side) and Plan 0341 (bypass side).
  The bypass v1 boundary (overflow-log-backed rows defer to the existing fold mechanism) and
  the undirected/vertex-purge remove paths (not wired; a bypass row being purged needs no
  compaction) are recorded in the Plan 0341 report.

**Delegation LANDED (2026-09-20, `ae25c9378`) — 614/0, with an insert-path regression to fix.** The rebalance now
goes through the shared pair (−152 lines) with the maintenance policy threaded explicitly
(`SpanResolutionPolicy::SlackMayBeDropped`), the publish owning the overflow-log fold for that path. Both stepped
tests are re-derived for that ownership (first step folds and reports the compaction move
`EdgeMoved { 1 → 0 }`, survivor visible at slot 0, then `EdgeMoved { 2 → 1 }`; the inline-property-bytes test
drops the variant claim about *who* folds and keeps both effects), and the slack test now runs on a roomy fixture
copied verbatim from the M1 gap test — `LabeledLaraGraph::new` takes **18** arguments and the fixture must
`push_vertex` before the first insert. Scoped fmt/clippy clean.

Focused canbench: `compact` 0 regressed (median −217.82 K instructions), but **`ins` 7 regressed — max +21.50 %,
p75 +3.69 %, median +0.61 %** — the cost of the up-front slack placement probe the "spare capacity is a hint"
rule runs on every `force_slack_grow` rewrite (`try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)`
before resolving). Candidate fixes, cheapest first: (a) probe only when the plan has not already chosen an
in-leaf base; (b) replace probe-then-resolve with resolve-then-downgrade — attempt `new_alloc` and on failure
retry once with `old_alloc` **without** letting the resolver relocate for slack; (c) memoise the probe's answer
inside the plan so the commit reuses it. Until one lands, the delegation is an architectural simplification with
a documented insert regression; the benchmark comparison must be re-run before any final artifact update, and the
ADR 0096 §5 row for "the rebalance is a rewrite" still needs to be added (the anchor text moved).

**Update on the `ins` regression (2026-09-20, `ae25c9378` + the probe-reuse commit).** Removing the duplicated
placement attempt (`perf(lara): reuse the slack placement attempt instead of resolving twice`) left the focused
canbench `ins` numbers unchanged (7 regressed, max +21.49 %, median +0.61 %), so the cost is not the probe: it is
the delegation itself. The shared pair takes more passes than the fourth implementation's tail — the planner
reads the buckets, walks each bucket's resident region *and* its log chain for sizing, decides the span, and the
commit rebuilds resident buckets, scans for the maximum run length and publishes descriptors — while the fourth
implementation's tail went straight from sizes to positions to writes. Next diagnostic, in one run: `canbench`
the single regressed bench with scope output and diff `labeled_rewrite_read_and_plan`,
`labeled_vertex_build_resident_buckets`, `labeled_vertex_calculate_bucket_positions`, `labeled_vertex_write_*`
against the pre-delegation baseline (HEAD~2) to see which pass dominates; only then decide between trimming that
pass, special-casing the no-log common case, or accepting the cost as the price of one implementation. The
delegation stays landed and green (614/0) meanwhile.

**`ins` regression narrowed to the tiling rebuild (2026-09-20).** Running `canbench ins` with scope output shows
**no scope changed by ≥0.5 %** (0 changed scopes), so the extra instructions are in *unscoped* code, not in
`labeled_rewrite_read_and_plan`, `labeled_vertex_build_resident_buckets`, `labeled_vertex_calculate_bucket_positions`
or `labeled_vertex_write_*`. The unscoped addition this session made on exactly that path is the tiling rebuild in
`commit_vertex_edge_span_layout`'s `!fold_logs` branch: it clones `resident_buckets` and rewrites every slab
bucket's width on **every** rebalance — which the insert path drives through the log-cascade — even though the
rebuild is a semantic no-op whenever no bucket keeps rows in its log (`bucket_resident_rows == stored_raw` then).
Fix to apply (small, verified-green target): guard that branch on
`buckets.iter().any(|b| !b.is_tiny_mode() && !b.is_tree_mode() && b.overflow_log_head() >= 0)` and fall back to the
plain `&resident_buckets` call otherwise (the code was `cargo fmt`ed since the draft patch, so match the formatted
form). Then re-run `canbench ins` and `canbench compact`; if the 7 regressions persist, the remaining candidate is
the plan/commit pass count itself and the decision becomes trim vs special-case the no-log common case vs accept.
State: delegation landed (`ae25c9378`), probe reuse landed (`26239c0b1`), tree 614/0, `compact` 0 regressed.

**Correction (2026-09-20): the tiling rebuild is not in the landed code.** The previous note blamed the
`!fold_logs` tiling rebuild for the `ins` regression, but that code only ever existed in the 612/2 per-caller
attempt, which was reverted before landing; the landed delegation (`ae25c9378`, built from the 611/3 policy state)
has no `fold_logs` threading at all (`grep -c fold_logs compact.rs` → the parameter appears only on the rebalance
entry point, and the rewrite still publishes with the fold). So the regression's cause is **not identified**, and
the scope evidence stands: no named scope moved ≥0.5 %, so the extra instructions are in unscoped code on the
delegated path. The unexplained numbers are `ins` 7 regressed, max +21.49 %, median +0.61 % (and `compact` 0
regressed, median −217.82 K instructions, i.e. the rewrite path got cheaper while the maintenance-driven insert
path got more expensive).

Next diagnostic, cheap and decisive, is a bisect rather than more reading: revert only `ae25c9378` (the shim) and
run `canbench ins` on HEAD~1; if that is clean, bisect inside the shim's parts (policy dispatch/threading → the
plan call → the finalize/release block) with one `canbench ins` each (≈2–3 min per run). Record which part carries
the cost before choosing between trimming it, special-casing the no-log common case, and accepting the cost as the
price of one implementation. Tree is 614/0 with the delegation landed.

**Bisect step 1 result (2026-09-20): most of the `ins` signal is not the delegation.** Reverting only the
delegation commit's file (`git checkout ae25c9378^ -- compact.rs`, everything else unchanged) and running
`canbench ins` gives **6 regressed, max +4.68 %, p75 +2.86 %, median +0.45 %**, against **7 regressed, max
+21.49 %, p75 +3.69 %, median +0.61 %** with the delegation. So the stored `canbench_results.yml` baseline
predates this session's other landings: roughly six small regressions (≤4.7 %) are shared/common, and the
delegation's own contribution is concentrated in **one bench at ≈ +17 %** plus ≈ +0.16 % on the median. A
per-bench parser attempt was unreliable (it reported only three *improvements* where the summary said six
regressions), so the next step is to read the bench names from canbench's own `--persist`-free output by eye, or
write a stricter parser over the `Benchmark:` blocks, and then look at that one bench's shape (likely a
cascade/maintenance-heavy insert where the plan + commit take more passes than the fourth implementation's tail).
Decision options once the bench is named: trim the pass it exercises, special-case the no-log common case, or
accept ≈ +17 % on that single workload as the price of one implementation. The delegation stays landed; tree
614/0.

**Scope-level bisect (2026-09-20): the dominant `ins` cost is the I2 drain walk, not the delegation.** Parsing
`canbench ins` per bench and per scope (saved run at `/tmp/ins_run.txt`) gives 82 regressed lines; the largest are
all in **`labeled_leaf_release_log_segment`**: +1627.87 % (`bench_l_ins_mv_l32_2048`), +580 % (the three
`bench_l_nt_bp_ins_*`), +454 % (`bench_l_ins_fresh_256`), +322 %/+316 %/+306 % (the three
`bench_labeled_stage2_hub_insert_grow_*`), +281 % (`bench_l_s2_h_ins_1024`, `bench_l_ins_sb_1024`,
`bench_l_ins_ex_bucket_128`), +231 %, +191 %. That scope wraps the release site, and the only thing this session
added there is invariant I2's guard — `ensure_leaf_overflow_logs_drained` walks every vertex of the leaf and reads
all of their buckets before releasing, where the release used to be a single store call. Bench-level totals are
correspondingly: `bench_l_ins_mv_l32_2048` +3.88 %, `bench_l_ins_last_1024` +3.87 %,
`bench_l_mix_hub_ins_33x50` +3.63 %, `bench_l_ins_rr_64l_1024` +2.47 %, `bench_l_df_ins_1024` +6.79 % (that one
from the same scope alone) — i.e. **most of the six pre-delegation-looking regressions are I2's guard**, not older
work. The delegation's own contribution is separate and narrow: +21.49 % on `bench_l_nt_bp_ins_*`
(`labeled_non_tail_bypass_insert` scope +21.37 %), the bypass insert path.

Fixes, in order: (1) short-circuit the drain check on `self.edges.overflow_log_segment_high_water(leaf) == 0`
(sound: no entries ⇒ no bucket can chain rows; the `bucket.rs` call site already tests that mark), which should
recover the whole `labeled_leaf_release_log_segment` group while keeping I2's guarantee; (2) look at the bypass
insert path's delegation cost separately (it is the one genuinely delegation-shaped number). Both were started but
not applied — the anchor greps for the drain function did not match (the signature is wrapped by `cargo fmt`, and
the function name should be re-confirmed before patching). Tree is 614/0 with the delegation landed.

**drain short-circuit landed (2026-09-20, `5283ad6d8`).** `ensure_leaf_overflow_logs_drained` returns early when
`overflow_log_segment_high_water(leaf) == 0` — a segment with no entries cannot have a bucket chaining rows in it,
and the bucket-side call site already tests the same mark. Measured with focused `canbench ins`: median +0.61 % →
**+0.09 %**, p75 +3.69 % → **+2.74 %**, max +21.49 % → **+19.32 %**; the `labeled_leaf_release_log_segment` scope
group (up to +1628 %) is what disappeared. What remains is the delegation's own cost in the bypass insert path
(`bench_l_nt_bp_ins_*`, `labeled_non_tail_bypass_insert` ≈ +21 %), the last open performance item from this
session, plus a residual ≈ +0.1 % median across the other benches. Suite 614/0, fmt/clippy clean.

**Last open performance item: the delegation's cost on the non-tail bypass insert (2026-09-20).**
`canbench bench_l_nt_bp` on the current tree shows `bench_l_nt_bp_ins_1024` **regressed by 19.32 %** at the bench
level, and inside it several scopes are marked **`new`** — work that simply did not exist in the baseline and that
the unified publish path introduced (the shared pair runs more scoped passes than the fourth implementation's tail:
resident-bucket rebuild, position calculation, run preparation and the write loops). So the delegation's residual
cost is *additional passes on the same buckets*, not a regression inside an existing pass; the pre-delegation
comparison in the earlier bisect already showed the shape of it (6 regressed without the delegation, 7 with it, and
the extra one being this bench). Next step, one command each: print the scope *names* alongside their deltas for
this bench (the previous parser dropped them) to see which new scope dominates, then decide between trimming that
pass, skipping it for the no-log common case, or accepting ≈ +19 % on this one workflow as the price of one
implementation — with `compact` (median −217.82 K instructions) and the overall `ins` median (+0.09 %) as the
counterweight. Everything else from this session is landed, green (614/0) and measured.

**Delegation cost attributed to the byte (2026-09-20).** Scope breakdown for `bench_l_nt_bp_ins_1024` (identical
for the 256/4096 variants), bench total 391.09 K instructions (+19.32 %):

| scope | instructions | delta |
| --- | --- | --- |
| `labeled_rebalance_leaf_cascade` | 194.09 K | **+47.06 %** |
| `labeled_rebalance_edge_log_vertex` | 185.89 K | **+50.07 %** |
| `labeled_non_tail_bypass_insert` | 386.53 K | +19.18 % |
| `labeled_rewrite_read_and_plan` | 129.25 K | **new** |
| `labeled_vertex_write_edge_runs` | 15.61 K | +52.35 % |
| (the rest) | ≈40 K | ≈ +100 % each on 0.7–6 K scopes |

So the cascade and the per-vertex log rebalance each roughly doubled, and the whole growth is accounted for by the
new `labeled_rewrite_read_and_plan` scope (129.25 K) plus the commit's write scopes: the fourth implementation's
prologue cost about half of what the plan now costs for the same decision (the cascade grew ~62 K while the plan
scope alone is 129 K, i.e. the plan replaced ~67 K of prologue and added ~62 K net). The plausible dominant terms,
worth measuring one at a time rather than guessing: (a) `bucket_resident_rows` walks the overflow-log chain for
every bucket even when the bucket has no log (`overflow_log_head() < 0` ⇒ `bucket_physical_resident_slots` is the
same number) — cheap guard, applies in the plan, the commit's tiling and the slide; (b) the plan and the commit
each read the vertex's buckets (`read_vertex_label_buckets`), so the same descriptors are decoded twice; (c) the
resolver is invoked even when the plan already knows the answer (an in-leaf placement for the current width).
Each is a one-line-to-few-line guard and each should be measured with `canbench bench_l_nt_bp` alone (~40 s). Only
after those numbers decide between trimming, special-casing the no-log common case, and accepting ≈ +19 % on this
one workflow — with `compact` at −217.82 K median and the overall `ins` median at +0.09 % as the counterweight.
Everything else from this session is landed and green (614/0).

**Where the plan's 129 K can and cannot be (2026-09-20).** Candidate (a) from the previous note is already handled:
`bucket_resident_rows` only calls `overflow_log_chain_len` when `overflow_log_head() >= 0`, so a log-less bucket
costs a field check plus `bucket_physical_resident_slots` — nothing to guard. So the plan scope's 129.25 K
instructions are in the rest of the plan, and the candidates worth one measurement each are: (i) the base
resolution the plan calls — `try_labeled_vertex_edge_base_in_pinned_leaf` performs a real placement attempt (free
span lookup) even when the span does not change; (ii) the plan runs *twice* per rewrite when the resolution moved
the vertex, because `rewrite_vertex_edge_span` re-reads the vertex and restarts (the restart is what the escalation
guard was for); (iii) `read_vertex_label_buckets` decoding the descriptors, which the commit then decodes again.
Method, one run each (≈40 s): wrap the three segments (bucket read, sizing/resolution, restart check) in
`#[cfg(all(feature = "canbench", target_family = "wasm"))] bench_scope(...)` the way the commit already does, run
`canbench bench_l_nt_bp` with `--persist` **not** set, and read which segment owns the 129 K. Only then choose
between trimming that segment, skipping resolution when the span is unchanged, or accepting the ≈ +19 % on this one
workflow. (Counterweight unchanged: `compact` median −217.82 K, overall `ins` median +0.09 %.) Tree 614/0,
delegation landed.

**Practical note for the three-segment plan measurement (2026-09-20).** The anchors are not unique by text: both
the plan and the `light` rebalance contain `let mut total_live = 0u32;` followed by a bucket loop with the same
`// ADR 0096 §5: tiny edges need no slab space` comment, so a text-anchored insert hits two sites and must either
disambiguate by line number inside `rewrite_vertex_edge_span_read_and_plan` (find the `fn` first, then insert
within its range) or by including a longer surrounding block. `let buckets = self.read_vertex_label_buckets(vertex)?;`
followed by `let old_alloc = vertex.stored_slots;` *is* unique and worked as an anchor. No edit was written (the
guards aborted before `write_text`), so the tree is unchanged and green. Method summary for the next attempt:
scopes `labeled_plan_read_buckets`, `labeled_plan_sizing`, `labeled_plan_resolve` inside the plan, then
`canbench bench_l_nt_bp_ins_1024` (~40 s) to see which owns the 129.25 K that the delegation added.

**Plan cost measured (2026-09-20): the base resolution owns it.** Three sequential `bench_scope`s inside
`rewrite_vertex_edge_span_read_and_plan` (`labeled_plan_read_buckets`, `labeled_plan_sizing`,
`labeled_plan_resolve`) report 144.19 K / 130.45 K / 122.59 K against the plan's own 151.12 K — canbench scopes
count from their creation to the end of the enclosing scope, so the segments are cumulative from each marker and
the *differences* are the segment costs: read ≈ 7 K, sizing loop ≈ 8 K, and **base resolution ≈ 122 K, i.e. 81 % of
the plan**. So the delegation's cost on the non-tail bypass benches is the placement attempt the plan performs
(`try_labeled_vertex_edge_base_in_pinned_leaf` through the resolver or the slack branch), not the descriptor read
or the resident sizing. That attempt is inherent to deciding whether a wider span fits, but it is about twice what
the fourth implementation's narrower attempt cost — the same decision, made with more machinery. Options, in the
order worth measuring: (1) skip resolution entirely when the plan's answer is the current span (`new_alloc ==
old_alloc && !compact`) — verify that path is actually reachable for these benches first, because if `compact` is
true there the packed width really does change and an attempt is needed; (2) make the attempt cheaper — the
resolver walks to a placement even when the vertex's existing cover already hosts the requested width, which
`old_alloc >= new_alloc` can answer without touching the leaf; (3) accept ≈ +19 % on this one workflow, with
`compact` at −217.82 K median and `ins` median at +0.09 %.

**Option 1 does not apply; the next scope split is inside the resolver (2026-09-20).** Reading the branch order in
`rewrite_vertex_edge_span_read_and_plan`: the cheap in-place answer (`(old_base, new_alloc)`) is taken when
`new_alloc == old_alloc`; the slack-hint branch (`try_labeled_vertex_edge_base_in_pinned_leaf` only, no
relocation) *additionally* requires `old_alloc >= min_required`, i.e. it only covers "content already fits, widen
for spare room"; a vertex whose content does not fit (`old_alloc < min_required`) falls through to the resolver,
which may relocate the leaf. So on the non-tail bypass benches the 122 K is either the plain placement attempt
inside the resolver or a leaf relocation it drives — the two have very different implications (the first is the
price of asking, the second is real maintenance work). Next measurement, one run (~40 s): put scopes around the
resolver's three steps — the initial `try_labeled_vertex_edge_base_in_pinned_leaf`, the
relocate-and-retry loop, and the policy tail — and read which owns the 122 K. If it is the initial attempt, the fix
is to answer "does the current cover already host the requested width" before touching the leaf; if it is the
relocation, the delegation merely moved work that the fourth implementation also did, and the honest comparison is
that scope against its baseline. Then choose trim / cheap-answer / accept (≈ +19 % on this workflow, with `compact`
−217.82 K median and `ins` median +0.09 %).

**The delegation's plan cost is one placement attempt (2026-09-20, final attribution).** Scopes inside
`resolve_labeled_edge_base_for_rebalance` on `bench_l_nt_bp_ins_1024` give `labeled_resolve_in_leaf` = **122.23 K**
with `calls: 1`, against the plan's 132.69 K total; the relocate and policy-tail scopes never appear (no relocation
happens on this bench). So the entire plan overhead is the single initial
`try_labeled_vertex_edge_base_in_pinned_leaf` query for the widened span — not the descriptor read (≈7 K), not the
resident sizing (≈8 K), not a repeated plan (the call count is 1), and not a leaf relocation. The fourth
implementation asked the same question with the same helper, so the remaining difference must be in the *inputs*
the two paths hand it (e.g. the plan resolving a wider allocation request than the fourth implementation's tail
did, hence a more expensive query) rather than in duplicated work. Options left, in order: (1) verify the requested
width the two paths pass for this bench (log `new_alloc` on both sides for one run) — if the plan asks for more,
that is the whole difference and it is a sizing question, not a placement one; (2) answer the query from data
already in hand when the current cover already hosts the request (`old_alloc >= new_alloc`), which the branch order
currently cannot reach because that branch only fires for growth; (3) accept ≈ +19 % on this workflow (counterweight
`compact` −217.82 K median, `ins` median +0.09 %). Diagnostic scopes have been removed; tree 614/0.

**Closing the performance item (2026-09-20): accept, with the numbers recorded.** The growth quantum was the last
code-shaped hypothesis — the plan asked for `next_vertex_edge_span_allocation(old, min_required, seg)` (≈ +25 %)
where the fourth implementation grew to `max(min_required, seg) + max(seg, base/8)` — and switching the plan to the
need-based quantum left the bench unchanged (`bench_l_nt_bp_ins_1024` +19.32 % → +19.42 %; overall `ins` median
+0.09 % both), so the change was reverted and the divergence from the `light` path's formula (which still uses
`next_vertex_edge_span_allocation`) is *not* introduced. Net state of the investigation: the delegation's cost on
this workflow is **one placement query** (`try_labeled_vertex_edge_base_in_pinned_leaf` = 122.23 K with `calls: 1`,
i.e. essentially the plan's whole 132.69 K), and it is *not* the descriptor read (≈7 K), the resident sizing
(≈8 K), a repeated plan, a leaf relocation, or the requested width. The fourth implementation asked the same
question through the same helper, so what differs is which answer the query gives for the plan's request — success
requires scanning for a suitable free span, failure returns early — which is a property of the request's fit, not
of the machinery around it. Recommendation: **accept** ≈ +19 % on `bench_l_nt_bp_ins_*` as the price of one
span-layout implementation, with `compact` at −217.82 K median and the overall `ins` median at +0.09 % as the
counterweight; revisit only if this workflow's absolute cost becomes a product concern (e.g. a different growth
policy for non-tail bypass inserts). Everything from this session is landed, green (614/0) and measured.

# Design reflection: is there a fundamentally better layout? (2026-09-20)

Every defect this session chased came from two root properties of the current layout, not from the bugs themselves:

1. **A vertex's span width must be predicted before the rows exist.** A bucket's rows live in a contiguous span
   whose width is chosen at rewrite time, so the design needs proactive slack, weighted re-tiling, leaf relocation,
   block sizing, placement queries and a "cover must be backed" rule (I1). The measured cost of that prediction on
   the non-tail bypass benches was one placement query = 122 K instructions, i.e. the *prediction* is the expense.
2. **The same rows have two representations.** A bucket's content is `slab prefix ∪ overflow log`, so the design
   needs chains, chain walks, folds, a fold prelude, a release protocol and the I2 rule — and the row-loss hazard
   this session found (`release_segment` zeroing a non-empty log; a stepped pack truncating an unfolded row) is
   possible only because two owners can disagree about where the rows are.

Both properties exist to serve one goal: **flat, contiguous sequential scans for traversal** (the "2 % parity"
dense path). So any redesign has to pay for locality explicitly.

## Candidate A′ — two storage classes only: inline (small) + chunked runs (everything else)

Delete the slab-prefix + shared per-leaf log + vertex-span triad; keep the inline tiny form and generalize the
existing chunked *tree* mode (LEG root + LTB blocks, ADR 0088) to every non-inline bucket.

* What disappears with it: I1 and I2 become vacuous (no cover, no releasable log), folds and the fold prelude, the
  log-full recovery loop, weighted re-tiling/slide, leaf relocation for span growth, slack sizing and its policy,
  `promote_*_to_slab` copies, and the tiling/positions machinery. That is the majority of what this session
  touched, including every hazard class found in it.
* What it costs: one indirection per chunk on scans (0 extra for inline, ~1 descriptor read per 64–256 rows above
  that), and it inherits the LTB/LEG machinery (which already exists and is already benchmarked — ADR 0088 has the
  tree-mode measurements). Growth becomes "allocate a chunk", so per-bucket waste is bounded by one chunk instead
  of by a predicted span; a delete can compact a chunk by moving at most one chunk's worth of rows.
* Fit with the workload: the Orkut census (`design/investigations/2026-09-17-lara-orkut-feedback-triage.md`, S3)
  shows K≤4 buckets are 69.6 % of buckets but only 25.4 % of edges at 1 M — i.e. **inline covers most buckets and
  all of the cheap ones**, while ~75 % of edges live in the buckets a chunked design would serve. So A′ changes
  the storage of the expensive minority and leaves the common case exactly as it is today.
* Risk to measure first, cheaply: the scan break-even between the current flat prefix and a chunked run per bucket
  size (tree-mode numbers in ADR 0088 are the starting point), and the insert cost of append-to-last-chunk vs
  slab-append. If the break-even is inside the measured distribution, A′ is a strictly better design; if not, keep
  the flat prefix for mid-size buckets.

## Candidate A″ — smallest change that kills the two root properties' worst consequences

Keep the flat slab prefix (and therefore scan parity) but **replace the shared per-leaf overflow log with
per-bucket spill chunks** (a chunk id + count in the descriptor, chunks from the same LTB store the tree mode
already uses).

* Kills: I2's class entirely (a bucket owns its spill; there is no shared area to release, no "was it drained?"
  question), the log-full recovery loop and its leaf-wide fold, the fold prelude, and the chain walks in sizing.
* Keeps: the prediction problem (spans still have widths) — so I1, slack policy and leaf relocation stay.
* New cost: a few bytes per bucket in the descriptor plus chunk-granular waste (bounded, and only for buckets that
  actually spill), and one indirection when a bucket has spilled.
* This is the option to prefer if the product wants a *bounded* step: it removes the hazard class and a whole
  recovery path for ~1 descriptor field and one store reference per bucket.

## What survives either way

The session's contracts are about *ownership*, not about the mechanisms they are implemented with, so they should
outlive a redesign: rows have exactly one owner at a time; a published width is a claim that must be backed;
releasing storage requires proving it holds nothing; slack is a hint and never a reason to allocate; and sizing
consumes one shared definition of "resident". A′ makes the first three vacuous by construction, which is the
strongest argument for it; A″ makes the second and third vacuous where they were actually violated.

# Combined design: per-bucket spill + lazy allocation (2026-09-20, after the A′ objection)

**A′ is out.** A chunked run for every non-inline bucket would put a degree-5 bucket on a whole chunk (1024 slots
of space for five rows), so generalizing the tree mode trades a prediction problem for a utilization problem. The
objection also sharpens the second candidate: A′ and A″ converge if and only if the run store supports *small*
runs, and if it does, the flat prefix should stay for mid-size buckets anyway (scan parity).

**A″ designed together with the dynamic-allocation item (GAP-006).** The two are the same change seen from two
sides — GAP-006 complains that log capacity is backed per segment whether or not it is used, and A″ says the log's
*ownership* is wrong (shared per leaf, releasable, hence the I2 hazard). Fixing either alone leaves the other; the
combined design is:

* **The descriptor field stays, its meaning changes.** `overflow_log_head` is an `i32` in a 29-byte row
  (`record.rs:576`), and a spill-run id fits it exactly. So the wire keeps its size: "index into the shared
  per-leaf log chain" becomes "id of this bucket's spill run (0/none)". No row widening, and the ordinal scheme
  (`stored + offset` beyond the prefix) is unchanged — reads and rank/select see the same logical slots.
* **Spill runs come from the run/block store the tree mode already uses** (LEG root + LTB blocks, ADR 0088), with
  **variable-size runs, doubling growth and a free list**. A bucket that spills five rows gets room for five, not a
  block — which is exactly the small-run support A′ lacks — and because runs are per bucket, a run is never
  contended: today one hot bucket can fill the leaf's shared log and trigger a leaf-wide fold that touches every
  neighbour, and afterwards the leaf's log segment is released for the whole leaf.
* **Allocation is lazy in both directions**: a bucket that never overflows its prefix has no run at all (the
  descriptor field is NONE), and the per-segment log store, its eager per-segment capacity, `release_segment`, the
  drain protocol, the fold prelude and the log-full recovery loop all disappear. That removes GAP-006's complaint
  and I2's hazard class together, because there is no shared area whose emptiness must be proven before freeing it.
* **Folding becomes per-bucket and optional**: when a rewrite can fit prefix + spill into the prefix it packs them
  (no leaf-wide pass, no policy tail, no "fold prelude"), and the freed run returns to the free list. When it
  cannot, the spill simply stays — growth is "extend my own run", which needs no prediction, no tiling, no cover.
* **What that leaves of today's machinery**: the resident-content definition (prefix + spill), "publish the width
  you materialized", I1 (a published width must be backed) and the slack rule all survive but shrink in scope —
  and every log-shaped contract (I2, the release protocol, the fold/recovery choreography) is deleted with its
  mechanism rather than kept as a rule about a mechanism that no longer exists.
* **Open questions to settle with measurements, not prose**: (i) small-run overhead in the run store (header per
  run — if it is heavy, the first spill level should be a packed small-run array rather than a general run);
  (ii) when a spill run shrinks or is released (natural answer: on the bucket's own compaction, when prefix + spill
  fit the prefix, and on bucket removal — with the free list absorbing reuse); (iii) insert cost of allocating or
  doubling a run versus appending to the shared log (the run is uncontended and amortized by doubling, the log
  append is cheaper per call but couples neighbours); (iv) scan cost of prefix + spill run versus prefix + chain,
  which should favour the run, since a run is contiguous where a chain is a walk.

Sequence if accepted: measure (i) and (iv) first (both are cheap, and (iv) is a win or a loss by a wide margin),
then change the field's meaning and the ownership, then delete the log store, the drain guard, the fold prelude and
the recovery loop.

# What `~/dev/lara` says about the log and dynamic allocation (2026-09-20, inspected)

The reference implementation settles two questions this session has been circling, with measurements:

* **The log is a performance mechanism, not a correctness one.** `crates/benches/tests/dgap_vs_log0.rs` runs LARA
  with `LaraConfig { max_log_entries: 0 }` ("log0") against the DGAP port and asserts identical neighbour sets, so
  the graph is correct with the log disabled. Their `docs/benchmark-log-2026-09-08.md` log-capacity sweep (T=1024)
  then ranks capacities: **cap 0 is the second-fastest on the hub fixture (314 µs) because "single-leaf fold acts
  bulk"**, caps 4–32 are *slower* than both ends (fold frequency plus chain cost), cap 512 is best of the tested
  range (170 is described as a PMEM legacy value, not an optimum), memory is linear in cap, and — importantly for
  GAP-006 — **persisted bytes per insert are flat across capacities** (hub 10.4–10.5, dispersed 8.2), so the log's
  size does not buy durability, only speed. Conclusion for us: making the spill lazy is about *not paying memory for
  a mechanism that may not be used*, not about correctness, and a blanket "smaller is better" would be wrong at
  small caps — the first spill should be sized to the bucket (power-of-two class), not to a leaf-wide constant.
* **Their allocator design for runs is exactly what A″ needs.** `docs/bucket-run-allocation-2026-09-13.md` (B2b)
  and `bucket-tail-growth-2026-09-13.md` (B3a) describe per-owner runs with **capacity = the smallest power of two
  that fits**, a **free list per capacity class** (the retired run's first 8 bytes hold the next link; `u64::MAX`
  terminates), **in-place growth only when `start + old_capacity == arena.len`**, and relocation otherwise — with
  measured wins on tail growth (32,768 → 65,536 descriptors: read requests 98,335 → 65,567, write requests
  65,541 → 32,772, mapped bucket bytes 2,359,296 → 1,572,864) and a stated cost for front insertion (the suffix
  shift remains). Mapped onto A″: the spill run *is* such a run, its capacity class gives bounded waste for small
  degrees (the objection that killed A′), and its growth is per-bucket, so no leaf-wide tiling, no prediction, no
  cover.
* **Their own docs flag what A″ fixes.** `docs/design-opportunities-2026-09-12.md` lists "chunk numbering" for the
  log as a candidate extension *blocked* by sharing one variable-size log area across users (alignment and
  co-location), and separately flags `layout_leaf`'s **minimum 1024-slot allocation per leaf** as a computed
  floor that costs ~16 MiB of edge space at 100 k vertices / 4 096 leaves. A″ removes the shared area (so chunk
  numbering becomes trivial — the descriptor's existing `i32` field becomes the run id) and the same
  power-of-two/capacity-class treatment would apply to leaf blocks later.

So the reference supports the direction, supplies a proven allocator shape, and supplies the measurement method:
their sweeps compare capacities on wall time, memory and persisted bytes per insert, which is exactly the triad to
measure for a per-bucket spill (plus the two extras noted earlier: small-run overhead, and prefix + run versus
prefix + chain scan cost).

## (iv) answered from the repo's own measurements (2026-09-20)

ADR 0088's Gate 2 verdict records the chunked-run (tree) full scan collapsing from ~14,540 ins/edge to **~41
ins/edge at both 4K and 65K** once the amortized chunk buffer replaced per-edge node traversal (a 351× change), and
notes that the "within ~20 % of slab baseline" gate was met trivially because the slab baseline number in that
comparison came from the *shared-leaf copy path* rather than from a per-edge scan. For comparison the rejected
B-tree prototype cost ~2,004 ins/edge. Two consequences for the design discussion:

* **A run/chunk structure's sequential scan is not the obstacle it looked like** — ~41 ins/edge with one
  dereference per 1,024 edges is the measured number for the structure A″ would reuse, so "prefix + spill run"
  does not lose the scan argument by a wide margin; the earlier `delete_half_by_slot_then_scan` blow-up in that
  table is explicitly a prototype artifact (naive O(N²) shifts), not the run structure.
* **The structure's real exposure is random ordinal access**, which the ADR calls out as block-transition and
  per-level-dereference cost. That is exactly why A″ keeps the *flat prefix* for mid-size buckets and uses runs
  only for spill: rank/select and positional reads stay flat where buckets are flat, and only the spilled portion
  pays an indirection.

So the remaining unknown is (i) — small-run overhead in the allocator — and the reference's own
`bucket-run-allocation`/`bucket-tail-growth` measurements already exercise per-owner runs with 24-byte records,
power-of-two capacity classes and a per-class free list, which is evidence that small runs are workable in that
design. Measure it in Gleaph anyway before choosing between a general run and a packed small-run array for the
first spill level, then proceed with the field-meaning change and the deletion of the log store, the drain guard,
the fold prelude and the recovery loop.

## (i) resolved by the store's granularity: the spill needs two levels (2026-09-20)

Gleaph's existing run store cannot serve small spill runs: `ltb_raw_block_store.rs` has a **fixed 4096-byte payload
per block** (`BLOCK_PAYLOAD_BYTES`, stride 4112 B) and `R_MAX = 1024` rows per block, with block ids dense over
that stride. A degree-5 bucket that spills would therefore occupy a 1024-row granule — the same waste that killed
A′ — and the store also has no capacity classes or free list (the reference's `BucketStore` does: smallest power of
two that fits, a free list per capacity class, in-place growth only at the arena tail). So A″ splits the spill into
two levels:

* **Level 1 — packed small runs owned by the bucket.** A dedicated small-run area with power-of-two capacity
  classes and a per-class free list, taken from the reference's proven allocator shape (`bucket-run-allocation`,
  `bucket-tail-growth`). This is what a bucket's first spill uses, so a five-row spill costs a small run, not a
  block — and it is also the level that makes lazy allocation meaningful, because a bucket that never spills has
  no entry at all.
* **Level 2 — LTB blocks (existing store).** Above a threshold where the 1024-row granule is proportionate, the
  spill continues in the tree store the tree mode already uses, which keeps promotion/demotion semantics and the
  measured ~41 ins/edge scan behaviour.

The transition between levels is the same shape the modes already use (inline → packed run → block), but it is
*per bucket and lazy* instead of per leaf and eager: no shared log, no per-leaf capacity, no release protocol, no
fold prelude, no recollection loop. What still needs measuring before implementation is the *sizing* of level 1
(chunk length and the class boundaries — the reference's data covers 24-byte descriptor records, not Gleaph's
4-byte edge rows) and the threshold at which level 2 takes over; both are one bench each with the triad
(wall / memory / persisted bytes per insert) the reference uses.

## Spill distribution measured: the design's premise, quantified (2026-09-20)

A one-off probe on a production-shaped hub (`build_mixed_label_hub(20, 500)`, 20 labels x 500 rows = 10 000 rows)
counted, per bucket, the slab width, the live rows and the overflow-log chain length:

```
buckets=20  log_backed=1  total_rows=10000  spilled_rows=74
chains = [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,74]
```

Three things follow directly:

* **Lazy allocation is justified quantitatively, not ideologically.** 19 of 20 buckets hold *zero* spill; only the
  one hot bucket spills, with 74 rows = 0.74 % of the rows. A per-leaf eager log capacity (Gleaph's 170 entries per
  segment) is therefore provisioned for buckets that never use it — which is GAP-006's complaint in numbers.
* **The shared area couples 20 buckets to one hot one.** Today that single 74-row chain lives in the leaf's shared
  log, so the next log-full event triggers a leaf-wide fold that touches all 20 buckets, and the leaf's log segment
  is then released for everyone. A per-bucket spill moves exactly 74 rows and touches nobody else — the coupling is
  the cost, not the row count.
* **Level-1 sizing starts from ~74 rows.** A power-of-two class of 128 covers the observed spill with <2x waste, so
  classes up to a few hundred cover this shape and LTB's 1024-row blocks take over only for genuinely large spills.
  The class boundaries still need a distribution over more fixtures (this is one shape at one moment), but the
  order of magnitude is now measured rather than assumed.

With this, the design question is closed on evidence: the log's ownership should be per bucket, its allocation lazy,
its small runs packed with capacity classes and free lists (the reference's allocator shape), its large runs the
existing LTB store, and its descriptor field the existing `overflow_log_head` slot reinterpreted as a run id. The
implementation is a multi-slice effort (small-run store, field meaning, then deletion of the log store, the drain
guard, the fold prelude and the recovery loop, followed by an ADR that supersedes the log's current description).

**Design slice 1 landed (2026-09-20, `f220205ad`): the packed small-run store.** `labeled/spill_run_store.rs`
implements fixed-width rows in power-of-two capacity classes (8..1024) with a free list per class — the
reference's allocator shape — verified by five tests: classes are powers of two bounded at both ends
(`class_rows(74) = 128`), rows round-trip, a released run is reused only within its own class, the arena grows by
the class capacity, and reopening the same memory keeps the arena and the free lists (`allocate` returns the run
that was released before the reopen, which is the property the descriptor will rely on). The module carries a
`dead_code` allow with a reason because it is deliberately not wired yet: the descriptor field reinterpretation,
lazy allocation on first spill, the LTB hand-off for large spills, and the deletion of the shared log store, its
per-segment capacity, the drain guard (I2), the fold prelude and the recovery loop are the following slices.
Suite 619/0 (614 + the 5 new store tests), scoped fmt/clippy clean, and the leftover `let mut new_alloc` from the
superseded slack rule is gone.

## Wiring decision for the spill store: no new memory region (2026-09-20)

The next slices need the small-run store to become part of the graph, and the obvious wiring — a seventeenth
`Memory` — would churn every construction site (`LabeledLaraGraph::new` appears at 16+ call sites across tests,
benches and the facade, each listing its memories positionally). Decision (superseded by the correction below: the spill store takes the **log's** region slot, which the same
change frees — see "What happens to the memory the log uses today"). The original note was: **the spill store lives
in the LTB region as a separate segment with its own header**, which is coherent because both levels of the spill are
spill storage — level 1 is the packed small-run arena, level 2 is the LTB blocks — and it keeps the graph's memory
count and every construction signature unchanged. The two allocators stay strictly separate inside that region
(each owns a segment, each has its own magic/version header, neither reads the other's bytes), so the only shared
thing is the region, not the accounting.

That makes the remaining slices small and ordered:

1. **Region + store construction**: give the LTB region a spill segment (header + arena from `spill_run_store`),
   construct the store in the graph's init path next to the LTB store, and prove persistence with a reopen test
   (the store test already covers the allocator's own reopen; the graph-level test covers the region split).
2. **Descriptor field**: reinterpret `overflow_log_head: i32` as a spill run id, keeping `ipb_log_len`-style
   length semantics for the spill (a spill is append-only, so the length is the live count and the capacity class
   follows from it — which is why the store's `release(run, len)` takes the length).
3. **Lazy allocation and the LTB hand-off**: the first row that does not fit the slab prefix allocates a level-1
   run (74 rows in the measured shape); a spill that outgrows the largest class (1024 rows) continues in an LTB
   block, and the descriptor gains a way to say which level it points at (one bit in the run id, since the id is
   a row offset and the top bit is free).
4. **Deletion**: the shared log store, its per-segment capacity, the drain guard (I2), the fold prelude and the
   log-full recovery loop all go, together with the ADR text that describes them.

## Is the spill store "just the log"? Role same, entity different (2026-09-20)

The spill plays the log's *role* — holding rows that do not fit the bucket's slab prefix — and it reuses the same
descriptor field (`overflow_log_head`, reinterpreted as a run id). Everything else differs, and each difference is
deliberate, because each one removes an owner or a choreography this session had to debug:

| | shared per-leaf log (today) | per-bucket spill run (A″) |
| --- | --- | --- |
| owner | the leaf; many buckets' entries interleaved in one segment | one bucket per run |
| shape | chain of entries (`prev` links), order recovered by walking | contiguous run; ordinal = offset (`stored + i`), no walk |
| free policy | one segment released for the whole leaf, after proving every bucket drained (I2, the drain guard) | released per bucket, when that bucket compacts or dies — nothing shared to prove |
| allocation | capacity pre-provisioned per segment (170 entries) as soon as the segment count grows | allocated on the bucket's first spill; a bucket that never spills owns nothing |
| growth | append anywhere in the shared segment until it is full | capacity class 8→16→…→1024 rows, then hand off to LTB blocks |
| compaction | leaf-wide fold: prepare, fold every vertex, release the segment, retry the insert | copy one contiguous run into prefix slack when it fits; skipping it is fine |
| failure modes | row loss if a release is not preceded by a proven drain; truncation if a fold is cut short | only its owner frees it, so those cases cannot arise |

Two clarifications worth keeping: the spill is **not** tree mode — tree mode remains the tier where a whole bucket
becomes chunked (LEG root + LTB blocks, ADR 0088), and its ~41 ins/edge scan is what level 2 of the spill reuses;
the spill run fills the *gap* between "fits the prefix (with slack)" and "should be a tree", which is exactly the
gap the log occupies today. And the tier list stays three deep, not four: inline (tiny) → slab prefix → spill
(small run, then LTB) → tree, with the spill being per bucket and lazy where the log was per leaf and eager.

## Does the spill design cover the dynamic log's fixed-capacity saving? Structurally, yes (2026-09-20)

GAP-006's audit measured the log's fixed cost precisely: a `segment16` graph with `uniform(256)` capacities and
5 000 vertices x 1 edge (5 000 buckets, 25 000 descriptor rows reserved, **zero slab cover, zero log entries
used**) had **87 040 log entries of capacity** — 170 entries per leaf segment x 8 B, backed as the segment count
grows, across 512 allocated segment-tree leaves. That is ~696 KB, matching the audit's page counts (empty 30 pages,
buckets 15, **log 11**, edges 5, vertices 2; one page is 64 KiB). So the fixed cost is per *segment*, i.e. it scales
with the **vertex count** and not with usage.

**As landed (2026-09-20, ADR 0097):** the edge log store and its segment table are deleted, so the
~696 KB of fixed log capacity is not merely unused - it does not exist. The spill region's only fixed
storage is the store header (64 B) plus eight per-class free-list heads (32 B); everything else tracks
spilled rows. The reviewer's space-audit re-run is the gate that confirms the ledger reads zero.

The spill design removes that cost at the root rather than tuning it:

* **Nothing is pre-provisioned per segment, because segments do not exist.** The only fixed storage is the
  small-run store's header (64 B) plus eight per-class free-list heads (32 B) for the whole region. The per-leaf
  log capacity, its segment table and its growth-by-segment-count behaviour all go with the log.
* **The cost tracks spilled rows, measured.** In the production-shaped hub only 1 of 20 buckets spilled (74 of
  10 000 rows), so 19 buckets would hold *zero* bytes; the one that spilled takes a 128-row class at 4 B per row =
  512 B, i.e. bounded by construction at under 2x the live spill. For the audit's shape (zero spilled rows) the
  saving is the entire ~696 KB, not a fraction of it.
* **No tuning trade-off is accepted in exchange.** The reference's cap sweep shows capacity is a speed/space knob
  with an interior optimum (mid caps 4-32 were *slower* than both ends; cap 0 was the second-fastest on the hub
  because a single fold acts as bulk) and that persisted bytes per insert are flat across capacity — so capacity
  buys speed only, and shrinking it is not free. The spill keeps the speed where rows exist (a contiguous run for
  the bucket that spills) and pays nothing where they do not, which is the one point cap tuning cannot reach.
* **One rule to keep it that way**: a spill run exists *only because rows exist* — no policy may pre-allocate spill
  runs for hot buckets "for growth", exactly as slack may not justify an allocation. That rule is the spill's
  analogue of the slack rule, and it is what makes this benefit structural instead of a tuning choice.

## What happens to the memory the log uses today (2026-09-20) — and a correction

Three distinct things are "the log's memory", and they end differently:

1. **Live rows** (the entries that actually hold edges: 74 rows for the one hot bucket in the measured hub shape,
   zero in the tiny-heavy audit shape). These are *content*, and they are re-created in their new home — the
   bucket's spill run, or the prefix when compaction fits — by the same rebuild that a layout change already
   requires. Per the repository's pre-production rule there is no migration code, no legacy decoder and no
   compatibility path: fresh state or reinstall, and the old graph is rebuilt with the existing transcription
   paths.
2. **Reserved-but-unused capacity** (the audit's 87 040 entries ≈ 696 KB, backed per segment as the segment count
   grew, with zero entries used). On a fresh graph this is *never requested*, so the saving is real pages and real
   cycles. Be precise about the limit: stable memory is never returned to the IC — `grow` only grows, and nothing
   in the design shrinks — so an *in-place* upgrade of an existing graph would not reclaim those pages. The
   pre-production reset is what makes this a saving rather than a relocation, which is exactly the rule AGENTS.md
   sets for layout changes.
3. **The log store's structures** (segment table, per-segment block sizing, chain walkers, the release
   bookkeeping, the fold prelude and the log-full recovery loop). These are deleted with the mechanism, and here is
   the correction: the earlier note said the spill store should live *inside* the LTB region to avoid a seventeenth
   `Memory`. Since this change also frees the log's own region, the better answer is to **repurpose the log's
   region slot as the spill store's own region** — the graph's memory count stays exactly the same (so no
   construction churn across the 31 `LabeledLaraGraph::new` sites) *and* the spill gets a clean region of its own
   instead of sharing bytes with the LTB allocator. That preserves the reason for the earlier decision while
   removing its downside.

Net: content is rebuilt, unused capacity is never allocated on a fresh graph, the store's bookkeeping is deleted,
and its region is handed to the spill store rather than dropped, so nothing in the graph's layout arithmetic
changes except the meaning of the bytes.

## Confirmed: the log is replaced wholesale by the spill (2026-09-20, pre-production)

No deployed data exists, so nothing is adapted and nothing is migrated: the log's mechanism is **deleted** and the
per-bucket spill takes its place. Concretely the change removes, rather than rewrites:

* `lara/edge/log.rs` and its inline-property-bytes twin — the segment store, its per-segment capacity, the chain
  representation (`prev` links, `overflow_log_chain_len`), `SegmentId::release_*` and the segment table.
* The log-specific graph paths: `prepare_vertex_edge_span_for_overflow_log_fold`, `rebalance_edge_log_*` (the
  leaf-wide recovery), `release_leaf_overflow_log` with the I2 drain guard, the fold prelude, the fold/deferred
  triggers that exist only to empty a shared area, and the log parts of the batch reservation.
* Their tests and benches (the `bench_l_du_log_*` family, the log-capacity fixtures, `overflow_log_same_leaf_*`),
  replaced by spill tests: lazy allocation on first spill, class reuse, level-2 hand-off, "no pre-allocation"
  regression, and the audit shape that must now show zero fixed cost.

What is kept because it is not the log: the inline (tiny) form, the slab prefix with the flat scan it buys, tree
mode for the moment, the descriptor's 29-byte row (its `overflow_log_head: i32` field becomes the spill run id), the
ordinal scheme (`stored + i` beyond the prefix), the resident-content definition (prefix + spill) and the ownership
rules (one owner per row; a published width must be backed; slack is a hint; a spill run exists only because rows
exist). The `Memory` region count stays unchanged — the freed log slot carries the spill store, renamed — so the 31
`LabeledLaraGraph::new` sites are untouched.

The gaps this session opened close by deletion rather than by fix, and their ledgers should say so: GAP-2026-09-20-001
(log loss while promoting), the log half of GAP-2026-09-20-005, and GAP-006 (eager per-segment capacity) all lose
their mechanism. Two design questions remain after the swap, in order: (i) whether tree mode is still needed once a
bucket can be "prefix + LTB-backed spill run" (tree's LEG root is a one-level block list, which a run header could
carry), and (ii) whether the inline threshold K should move now that the middle tier is lazy. Both are follow-ups
with their own measurements, not part of the swap.

# The spill design, in one place (2026-09-20)

## Purpose
A bucket's rows must be reachable in order, and its live count grows unpredictably. The slab prefix is a
contiguous run sized in advance, so rows beyond it need somewhere to live. Today that place is a *shared per-leaf
overflow log*: a chained, pre-provisioned, leaf-owned area that is emptied by a leaf-wide fold and released as a
whole. The spill replaces it with a **per-bucket, contiguous, lazily allocated run**.

## Data model
* Tiers: **inline** (tiny, K<=4, rows live in the descriptor) -> **slab prefix** (a contiguous span, sized with the
  existing policy) -> **spill run** (this design) -> **tree** (unchanged for now; whole-bucket chunking).
* A bucket's logical row `i` is the prefix slot for `i < prefix_len`, otherwise spill row `i - prefix_len`. That is
  the same ordinal scheme the log used (`stored + offset`), so **ordinals do not move when the spill grows**: the
  spill is append-only.
* Descriptor: the existing 29-byte row, with `overflow_log_head: i32` reinterpreted as the **spill run id**. No row
  widening. A run id is a row offset in the spill arena, so its top bit is free to mean "level 2" when the spill
  continues in LTB blocks.
* Length: because a spill is append-only, the descriptor's live length *is* the run's used length, and the run's
  capacity class follows from it — so the descriptor needs **no capacity field** (`release(run, len)` recomputes the
  class).

## Operations
* **Insert.** (a) a free prefix slot (tombstone reuse, or the Insertion headroom) is used in place; (b) else, if the
  bucket's run has room, append to it; (c) else allocate a run of the next class, copy the old rows, release the old
  run, append. Beyond the largest class (1024 rows) the spill continues in LTB blocks. Nothing is pre-allocated, no
  other bucket is touched, and there is no leaf-wide pass — so the log-full error and its recovery loop have no
  analogue: the only failure is a genuine allocation failure.
* **Scan.** Prefix rows in order, then the run's rows in order — contiguous, one read per run (one per block at
  level 2), no chain walk; descending is the same in reverse. Ordinals come out identical to the log's.
* **Delete.** A layout-native tombstone is written in place (prefix or spill) and the live count drops; the run's
  length does not shrink. When the bucket's live rows fit the prefix again, compaction copies the spill's live rows
  into the prefix and **releases the run** to its class free list — the only place a run is freed, together with
  bucket death.
* **Compaction is optional.** If the rows do not fit the prefix, the compaction is simply skipped; nothing else
  depends on it happening.
* **Reopen.** The store's 64-byte header plus eight per-class free heads is the whole persistent state; runs are
  addressed by row offset, so nothing is rebuilt (the store test proves a released run is returned after reopen).

## Allocator
Power-of-two capacity classes (8..1024 rows), a free list per class whose first four bytes hold the next free run's
row offset (`u32::MAX` terminates), reuse only within the same class, and no coalescing. Growing past a class
allocates a new run, copies at most 2x the live rows and releases the old one, which is the standard doubling
argument: amortized O(1) per insert, with waste bounded by the class (<2x).

## Memory behaviour
A bucket that never spills owns nothing (measured: 19 of 20 buckets in the production-shaped hub; the audit's shape
had zero entries used). A bucket that spills pays its class: 74 rows -> a 128-row class = 512 B at 4 B per row. The
fixed per-segment cost disappears entirely (the audit measured ~696 KB of log capacity for 5 000 vertices with zero
usage). Stable memory is never returned to the IC, so this is a saving on a fresh graph and a rebuild question on an
existing one — which the pre-production rule already answers.

## Invariants
One owner per row at a time; a published width (and a spill length) is a claim that must be backed; **a spill run
exists only because rows exist** (no policy may pre-allocate one for growth — the slack rule's analogue); ordinals
of existing rows never change; resident content is `prefix + spill`, one definition shared by sizing, placement and
publish.

## What this deletes, and why each deletion follows from one property
| Deleted | Why it existed | Why the spill does not need it |
| --- | --- | --- |
| chain (`prev` links, chain-length walks) | one shared area held many buckets' entries interleaved | a run belongs to one bucket and is contiguous |
| fold + fold prelude | the shared area had to be emptied before it could be reused | freeing is per bucket, so nothing to empty |
| drain proof + the I2 guard | a release freed an area other buckets might still be using | only the owner frees its run, and only when it is done |
| log-full error + leaf-wide recovery | one bucket could fill an area everyone shared | a bucket extends its own run |
| per-segment capacity and its segment table | segment granularity, provisioned eagerly | lazy, per-bucket, class-sized |
| part of the span-growth machinery | the prefix width had to be predicted | growth is "extend my spill", which needs no prediction |
| part of the slack policy | prediction needed spare room as a cushion | the spill absorbs variance; slack stays a hint |

## Numbers that make it concrete
41 instructions per edge for a chunked sequential scan (ADR 0088's tree measurement, the structure level 2 reuses);
74 rows spilled by the single spilling bucket in a 20 x 500 hub; 128-row class for it (<2x waste); 8-row smallest
class; 1024-row largest class before LTB; ~696 KB of per-segment log capacity removed in the audit shape; the
reference's cap sweep (cap 0 correct and near-fastest, persisted bytes flat across cap) showing capacity buys speed
only.

## Open questions (each with a measurement, not an assumption)
Level-1 class boundaries against Gleaph's 4-byte rows (the reference's numbers are for 24-byte records); the level-2
threshold; whether tree mode is still needed once a bucket can be "prefix + LTB-backed spill run" (tree's LEG root
is a one-level block list a run header could carry); whether K moves now that the middle tier is lazy; and the
batch/deferred paths, which lose their log-capacity reservation entirely.

## Clarification: what a "class" is, and why the length can determine it (2026-09-20)

A **class** is one of the run capacities a spill may be allocated with — the powers of two from `MIN_ROWS = 8` to
`MAX_ROWS = 1024` rows (`CLASS_COUNT = 8`), i.e. 8, 16, 32, 64, 128, 256, 512, 1024 rows. `class_rows(len)` picks the
smallest class that holds `len` (so 74 rows -> 128), and `class_index(capacity)` maps a class to its free list.
Runs are *only* allocated with these capacities, which is what makes reuse possible at all: a freed 128-row run can
serve any later spill needing 65..128 rows, and the free list is an O(1) stack per class.

Two properties make it cheap rather than a bookkeeping burden: waste is bounded by doubling (<2x, measured: a
74-row spill in a 128-row class), and the whole structure is 8 free-list heads = 32 bytes of header. A denser
ladder would waste less but needs more classes and fragments reuse across them; a sparser one wastes more.

One precision that matters, because it is why the descriptor needs no capacity field: **a spill run's used length is
monotone**. Deletes write a tombstone in place and do *not* shrink the length, and rows only leave the spill when
compaction copies them back into the prefix — at which point the run is released with exactly the length it has. So
`ceil_pow2(length)` always equals the capacity the run was allocated with, and `release(run, len)` recomputes the
right class from the length alone. Had deletes shrunk the length, the class would have to be stored (which is what
the reference does for its descriptor runs); with append-and-tombstone semantics it does not.

## Which tiers use the level-1 spill arena (2026-09-20)

Correct: **tiny and tree do not use it; only slab does.**

* **tiny**: rows live inside the descriptor (K<=4). No prefix, no spill. Growing past K promotes to slab, and the
  new slab bucket starts with its prefix holding the transcribed rows and an empty spill.
* **slab**: rows = contiguous prefix + per-bucket spill run (level 1 arena, then LTB blocks at level 2). This is the
  only tier the level-1 arena serves, and the only tier whose descriptor field means "spill run id".
* **tree**: rows already live in chunked runs (LTB blocks referenced from the tree's own root in the descriptor), so
  a tree bucket **is** chunked storage — it needs no level-1 spill. In tree mode the descriptor field keeps the
  meaning it has today (the tree's own root/run reference), and the redesign does not change it.

The nuance worth keeping: "spill" as a *concept* — rows beyond a contiguous prefix, held as chunks — already exists
in tree mode; what the redesign adds is the **level-1 arena** for the middle tier, between "fits the prefix" and
"should be chunked outright". That is also why the open question about tree mode is not "does tree need a spill"
but "can a slab bucket's spill grow into LTB blocks indefinitely, making tree mode unnecessary" — a unification
question with its own measurement, not part of the log-to-spill swap.

Transitions change shape in exactly one place: every path that reads "the rows beyond the prefix" (slab->tree
promotion, demotion, compaction, the resident-content definition used by sizing and by the publish) reads the spill
run instead of walking a log chain. Reads become contiguous, and each of those paths loses its chain-handling.

## When rebalance/relocate happen after the spill (2026-09-20)

**A full class does not trigger a rebbalance or a relocation.** A run that outgrows its class takes the next class:
allocate, copy at most 2x the live rows, release the old run — a per-bucket step whose only cost is that copy
(amortized O(1) by doubling). It touches no other bucket, no span, no leaf, and needs no policy.

What still triggers leaf-level work, and why each survives the spill:

| Trigger | Why it survives | Frequency after the spill |
| --- | --- | --- |
| A **new bucket** (a vertex gains a label) needs room for its prefix quota in the leaf | prefixes still live in the leaf block; the spill only holds rows *beyond* a prefix | unchanged — this is structure, not growth |
| **Leaf density** crossing the block/density thresholds | the leaf's block is still the backing for the prefixes it holds | lower: content growth no longer inflates prefixes |
| **Explicit maintenance** (defrag, batch reservation expansion) | unchanged | unchanged |
| **Mode transitions** (tiny->slab at K+1, slab->tree at T_promote, tree->slab demotion) | unchanged; they now transcribe `prefix + spill` instead of `prefix + log chain` | unchanged |

What still triggers a **vertex-span rewrite** (rebalance in today's sense):

* a **new label** joining the vertex's bucket list (descriptor rows plus a prefix placement);
* **compaction** — copying the spill's live rows back into the prefix when they fit, which is per bucket, optional,
  and the only place a run is freed besides bucket death;
* **slack/density policy** when it wants a different prefix width for a vertex (today's weighted re-tiling), which
  becomes rarer because a tight prefix now simply spills sooner instead of needing to grow;
* the mode transitions above.

The measured shape shows the size of the change: in the 20 x 500 hub, **1 of 20 buckets** broke its prefix (74 of
10 000 rows). Today that one bucket's growth drives leaf-wide folds, leaf-level span growth and segment releases,
touching 19 buckets with no spill; after the change it drives one run growth of at most 128 rows. Leaf-level work
is left for structure (new labels, density, mode changes) rather than for content.

Two interactions this raises, which belong in the implementation slices rather than in this answer:

1. **Hole reuse in the spill.** Today `Unordered` fills the first tombstone hole, and `Insertion` dense-appends
   into a `used` headroom. A spill that is append-only with in-place tombstones supports the second trivially but
   must decide how the first finds a hole: a bounded scan, or a per-bucket free-slot hint in the descriptor. This
   is the spill's analogue of the tombstone-reuse rule in ADR 0052 and should be stated in ADR 0097 before the
   insert path is rewritten.
2. **Middle-ordered inserts.** With rows split across prefix and spill, an insert that must land at a specific
   ordinal may have to shift part of one or both. The current design avoids most of this by reusing holes and
   appending at the dense end, so the question is whether the same discipline holds once the dense end is a run —
   it should, but the rule needs to be written down so the semantics do not drift.

## Where the spill is lighter, and where it is not (predictions to check, 2026-09-20)

Lighter, and by construction:

* **Insert**: no leaf-wide fold, no log-full error, no recovery loop, no shared-capacity contention, no chain
  append. The path becomes "prefix slot if free -> append to my run -> grow my class". The trigger for the workload
  coupling disappears: one bucket's pressure no longer reaches its 19 neighbours.
* **Read**: prefix rows then a **contiguous** run, instead of prefix rows then a chain walk. The same improvement
  applies to every consumer of "rows beyond the prefix": slab->tree promotion, demotion, compaction, the
  resident-content definition, and the log-specific fold paths that go away with it.
* **Random ordinal access improves as well**, which is worth stating because the ADR calls it the structure's real
  exposure: today an ordinal inside the log is reached by chasing `prev` from the head (O(entries)), while a spill
  row is `run + offset` (O(1), plus one block hop at level 2).
* **Batch** loses its log-capacity reservation entirely — an admission decision, a rollback path and a class of
  failure modes go with it.
* **Maintenance** drops from "content growth" to "structure" (new labels, density, mode changes), and each event is
  smaller: the measured hub shape had one growing bucket out of twenty, and it drove leaf-wide work.
* **A concrete regression should disappear**: the focused canbench cost that this session attributed to the plan's
  base-resolution query (`labeled_resolve_in_leaf`, 122 K of the non-tail bypass insert's bench) exists because a
  span must be *found* when content outgrows its prefix. With the spill that growth is unnecessary for content, so
  the query is not made. That is a prediction the implementation must confirm, not assume.

Not lighter, or newly worth designing away:

1. **Growth copies.** The log appended in O(1) with no copy; a spill that outgrows its class copies up to 2x its
   live rows (amortized O(1), but a spike at each growth). Mitigation to design in rather than discover: allow
   in-place growth when the run is the arena tail, which is the reference's B3a rule and was measured as a win
   there (its `bucket-tail-growth` reduced read/write requests and mapped bytes on exactly this pattern).
2. **Hole reuse inside the spill.** Today `Unordered` fills the first tombstone hole in the prefix and the log
   merely appends. With rows split across prefix and spill, the same rule must be defined for the run — a bounded
   scan (a new per-insert cost) or a free-slot hint in the descriptor (O(1)). The hint is the right answer if the
   measurement shows the scan matters; either way the rule belongs in ADR 0097 before the insert path is rewritten.
3. **An allocator to maintain** — eight class free lists, a few reads and writes per allocation. Small and fixed,
   and strictly less bookkeeping than the segment table, the per-segment capacity and the release protocol it
   replaces.
4. **Class waste** is bounded at <2x on *spilled* rows only (measured: a 74-row spill in a 128-row class = 512 B),
   against the log's eager per-segment capacity (~696 KB unused in the audit shape) — an improvement in the
   measured shapes, not a guarantee in all shapes, so the audit should be repeated after the swap.

Net: the hot paths lose a level of indirection, a choreography and a coupling; the costs that remain are bounded,
per bucket and measurable, with two to design deliberately (in-place tail growth, hole reuse).

# File-by-file plan for the log-to-spill swap (2026-09-20)

The swap is atomic — the layout, the producer, the consumers and the tests move together — but it is reviewable as
the following file-by-file work. Reference counts are log-related references measured today (18 files, ~450 sites).

| File (log refs) | Change |
| --- | --- |
| `labeled.rs`, `lara/edge/init.rs` | The log store's construction becomes the spill store's; `SegmentId`/`release_*` and the segment table go. The region slot the log occupied carries the spill store, so the memory count is unchanged. |
| `record.rs` (52) | `overflow_log_head` -> `spill_run` for slab (tiny keeps its payload meaning, tree keeps its own root reference); `with_overflow_log_head` -> `with_spill_run`. 29-byte row unchanged. |
| `lara/edge/log.rs` (+ `edge_inline_property/log.rs`) | Deleted with the chain API: `overflow_log_chain_len`, the asc-index walk, `read_overflow_log_entry`, the append path, `overflow_log_segment_high_water`, `read_overflow_log_state`, `release_log_segment`, `log_entry_stride`. Replaced by the spill store's `allocate` / `write_row` / `read_row` / `release`. |
| `insert.rs` | The overflow decision becomes "prefix slot (hole or `used` headroom) -> append to my run -> grow my class", with in-place tail growth per ADR 0097. No leaf-wide pass, no error path. |
| `remove.rs` (48) | Deleting a spill row writes a tombstone in place; the chain-splice logic (`prev` relinking, head rewrites) disappears — a real simplification, since a run needs no unlinking. |
| `traverse.rs` (78), `iter.rs`, `slot_index.rs` (12) | Chain iteration becomes indexed run iteration; the ordinal algebra (`stored + offset`, tombstone-inclusive positions) is unchanged, which is what keeps reads observable-compatible. |
| `compact.rs` (73) | Delete the fold, the fold prelude, the leaf-wide recovery and the drain guard (I2); add per-bucket compaction (copy the run's live rows into the prefix, release the run). The resident-content definition reads `prefix + run` and loses its chain-length walks. |
| `batch_write.rs` (41) | The log-capacity reservation and its rollback path become "the run may need to grow"; the reservation must account for the possible class-growth copy. |
| `values.rs` (33) | The inline-property-bytes log is the same problem with a second row width, so it gets the same treatment (its own arena/store instance; the region is shared in segments, the allocators are not). |
| `bucket.rs` (35) | Head-based anchor/successor logic becomes run-based. |
| `invariants.rs` (4) | Log checks become spill checks: a run's used length never exceeds its class, and a published length is backed (I1's analogue for the spill). |
| `tree_write.rs`, `tree_read.rs`, `promote.rs` | Transcription reads `prefix + run` instead of `prefix + chain`. |
| `deferred.rs`, `bench.rs`, `batch_write_test.rs` (4/12/28) | Triggers that existed only to empty the shared area are deleted; benches and tests are replaced by the spill set (lazy first allocation, class reuse, tail in-place growth, level-2 hand-off, no pre-allocation, audit shows zero fixed cost). |

Three parts need care rather than bulk: the inline-property-bytes twin (same design, second arena), the batch
reservation (growth must be reserved or rolled back), and `slot_index.rs` (the ordinal algebra is the contract that
keeps the swap invisible to readers). Recommended slice order: (1) layout/region + store construction with a reopen
test, (2) descriptor rename and the insert producer, (3) reads and the residents, (4) removes and compaction,
(5) batch and the values twin, (6) deletion of the log API and its tests, (7) ADR 0096 §5 / `design/storage/lara.md` /
D-GAP contract text, then the audit re-run (the 696 KB must read zero) and the full gates.

# Spill swap landed (2026-09-20, ADR 0097) — branch `spill-swap`

The log-to-spill swap is implemented in this worktree as one ordered series, one mechanism per
commit: the store in the log's region and the descriptor field (S1-S3), the delete path
(`BucketEdgeDeleteLocation::Spill`), the placement surface and the labeled CSR accessor, the spill
wrapper contract tests, the producer plus the single mode-generic promotion entry, the deletion of
the fold/drain/segment-release choreography, the compaction and resident-definition readers, the
hybrid inline-property replay and the traverse readers, the batch boundary, the edge log store's
deletion, the packing/order defects the running suite exposed, and the promotion's two-plane
compaction. `labeled/graph/spill_swap_test.rs` carries one witness per mechanism.

Gates at the branch head: `cargo check -p ic-stable-lara --lib` 0 errors; `cargo test -p
ic-stable-lara --lib` 621 passed / 0 failed; `labeled::spill_run_store` 21 passed; `cargo fmt -p
ic-stable-lara` clean; `cargo clippy -p ic-stable-lara --lib` no crate warning. G6 byte shapes were
**measured** and disagree with the derived table - the wider 36 B descriptor and 25 B vertex rows
straddle page boundaries: tiny insert 150R/44W (derived 149/44), tiny delete 109R/44W (105/44), tiny
scan 109R/0W (105/0), slab control 425R/73W (411/72). Each test records the measured value and the
disagreement it replaces.

Defect found by the canbench gate after F4d and fixed in F7 (GAP-2026-09-20-007):
`bench_l_nt_bp_ins_1024` trapped with `CollectAllocationOverflow` in the leaf cascade's
span rewrite for a bucket that owns a spill run, while the same fixture passed on `main`.
The fix applies the owner's decision (b) - a relocation keeps the run, so the rewrite no
longer asks the span for the run's rows - and the bench's own shape is its witness. The
measured cost of the repaired path is recorded in the GAP entry.

Deviations recorded rather than absorbed: (1) the batch defers a run that would spill to the scalar
path (a recoverable `SpillRunDeferred`) instead of reserving a class-growth copy, because
`EdgeStore` has no allocate-without-copy and a reserve-time growth would free the run the descriptor
still points at; (2) the promotion compacts both planes before transcription instead of reading
`prefix + run`, because the values plane still keeps its own log (slice 5); (3) wide-edge buckets
keep the slab-growth recovery instead of promoting; (4) the ADR's final 34-byte row and the values
twin's deletion (slice 5) are not part of this swap, so the row stays at the interim 36 bytes.

# Session handover index (2026-09-20) — read this first

**Verified state**: 621 passed / 0 failed; `cargo fmt` and scoped `clippy --lib` clean; the only uncommitted files
are the twelve foreign ones listed at the top of this ledger's stash-incident note.

**Landed this session (code)**: single publish path for rewrites (−147 lines); the rebalance delegating to it
(−152 lines); one definition of a bucket's resident rows; placement preference wired into the commit; **I1** (a
published span width is the width the resolution obtained); **I2** (a log segment is released only when the leaf is
drained) plus its short-circuit; named span-resolution policy shared by both resolvers; a spanless vertex never
requests slack (class C); spare capacity is a hint, never a reason to allocate; the light rebalance contributing zero
resident for tiny; slack placement attempt reuse; and the first two slices of the spill design
(`spill_run_store.rs`: power-of-two classes with per-class free lists, reopen persistence, in-place tail growth, 7
tests).

**Measured performance**: `compact` 0 regressed (median −217.82 K instructions); `ins` median +0.09 % with one
workflow at ≈ +19 % (`bench_l_nt_bp_ins_*`), attributed scope by scope to the plan's base-resolution query
(`labeled_resolve_in_leaf` = 122 K, `calls: 1`); ADR 0097 predicts that cost disappears once content growth goes to
a spill run instead of a span.

**Designed, not implemented**: ADR 0097 (proposed) replaces the shared per-leaf overflow log with per-bucket spill
runs — see "The spill design, in one place" for the model, "File-by-file plan for the log-to-spill swap" for the
work list and slice order, and "Confirmed: the log is replaced wholesale" for what is deleted. The swap is **one
atomic change**: the log's region, the descriptor field's meaning, the insert producer, the readers, the compaction
paths, the batch reservation, the values twin and the tests move together, because the spill cannot share the log's
region while the log still exists. That is why no further code was started here: a half-applied swap would leave the
tree red for the duration.

**Open items, in the order worth doing**: (1) execute the swap in the slice order recorded above, then re-run the
space audit (the 696 KB must read zero) and check the predicted disappearance of the +19 %; (2) the three
ADR-0097 measurement questions (level-1 class boundaries against 4-byte rows, the level-2 threshold, hole reuse in
the spill); (3) whether tree mode survives the spill's level 2, and whether K moves; (4) the final gates:
unfiltered `canbench --persist`, the PocketIC boundary test, and the design-doc text updates (ADR 0096 §5's log rows,
`design/storage/lara.md`, the D-GAP contract).

**Artifacts in `/tmp` (volatile)**: `delegation_policy_v2_compact.rs` (the 611/3 intermediate, superseded by the
landed delegation), `delegation_foldpercall_612_2_*` (the per-caller experiment that exposed the visibility loss),
`head_before_reconstruct.rs`, and the earlier compaction/unification variants. Nothing there is needed to continue —
the landed code is the current state, and the design lives in this ledger plus ADR 0097.

## Prepared workspace for the swap, and why it is not started here (2026-09-20)

A worktree at the current commit is prepared at `~/dev/gleaph-spill` (`git worktree add ~/dev/gleaph-spill HEAD`,
detached at `0d176b579`), so the atomic log-to-spill swap can be executed without putting `main` into a red state
for its duration. Remove it with `git worktree remove ~/dev/gleaph-spill` when the work is done.

The swap is not started in this session, deliberately. Re-checking one more time for a safe first slice: the spill
store needs the log's memory region, and there is no other region to take (the LTB region's allocator has no
capacity ceiling, so a reservation at its end would be overwritten when the tree store grows; the graph's sixteen
memories are all in use and adding a seventeenth churns the 31 `LabeledLaraGraph::new` sites for a structure nothing
can use yet). The log store itself is constructed by the *edge* store (`lara/edge/init.rs`), not by the graph, so
even the layout step touches a type whose log API is used from eighteen files. Every road leads to the same
conclusion: the layout, the descriptor's field meaning, the producer, the readers, the compaction paths, the batch
reservation, the values twin and the tests change together, and stopping halfway leaves the tree unusable.

That is not a defect of the design — it is one atomic change with a complete contract (ADR 0097) and a complete
work list ("File-by-file plan for the log-to-spill swap", including the recommended slice order). It is simply work
that wants a fresh, focused session rather than the tail of this one, and the session's job has been to make it
cheap to start: the problem is measured, the alternatives are argued, the invariants and the deletion list are
written down, the allocator's first two slices are implemented and tested, and the workspace is prepared.

## Delegation record: the swap is being implemented outside this checkout (2026-09-20)

The log-to-spill swap is delegated to the Herdr agent `gleaph-lara-impl` (pane `w1:pS`, kind `pi`, cwd
`/Users/yota/dev/gleaph`), which was renamed from an unnamed pane for this purpose via
`herdr agent rename w1:pS gleaph-lara-impl` (the name had not reached Herdr's server state before that, which is why
`herdr agent get gleaph-lara-impl` initially returned `agent_not_found` while `herdr agent list` showed only
`vecdeque-impl` and the reference-repo `lara-impl` in `w2:p2`).

Its instructions are the file `~/dev/gleaph-spill/SPILL_SWAP_BRIEF.md`: work only in the prepared worktree
(`git worktree add ~/dev/gleaph-spill HEAD`, detached at `0d176b579`), start a `spill-swap` branch, follow ADR 0097
as the contract and the file-by-file plan as the work list, keep the two existing spill-store slices, and report per
slice with evidence (test counts, gate output, the GAP-006 audit re-run showing zero fixed log capacity, and the
`bench_l_nt_bp` number). The brief also carries the rules this session learned the hard way: no `git stash`, no
tree-wide state operations, scoped `fmt`/`clippy` only, never weaken a test to make an implementation pass, and
re-derive a test with its justification when a mechanism changes.

This checkout stays the reviewer's copy: `main` remains at 621/0 and must not receive any of the swap's intermediate
states. The review gate on its report is: re-run `cargo test -p ic-stable-lara --lib`, scoped `fmt`/`clippy`, the
audit probe, and `canbench compact`/`ins`; only then decide which commits land, in code-then-docs order.
