# ADR 0097 — Bucket-segment publication preflight

**Status:** Partially Implemented (ADR0097); descriptor-segment slice0386 is implemented, validated
and independently reviewed after0393 diagnostic closure. Strict LARA lib/test Clippy, native708
and workspace formatting pass; retained-test and cleanup review verdicts are APPROVE. Performance
is **ACCEPTED WITH KNOWN REGRESSION** by the user's safety-first decision below; residual optimization
is ended. Later tiny/leaf/queue/Graph/width work remains planned;0394's native gate now requires the
existing physical-layout prerequisite0395 before geometry implementation.0395 completes its physical-only
implementation/review gates separately, with explicit user acceptance of its measured prefix +2.0167%
and final structural/preservation evidence below.0396 now closes the separately reviewed mode/count/
range prerequisite;0394 geometry remains STOPPED pending a fresh723-native/performance baseline.
Its Q1 approval remains draft-boundary approval only; no validation gate is waived.

Normal segment rewrites and bypass conversion now use one `LabelBucketStore` preparation and
consuming publisher. Layout V1, leaf geometry, outer graph/PMA retirement, queue ownership and
width declaration are unchanged. No stage, commit or push was performed. The prerequisite 0385
commit remains `9f42b4868fad635fe7423fcea07ad7af41699edb`, with its validation-partial status intact.

## Performance acceptance

User accepts the retained safety implementation and its observed+3.8–6.4% scalar instruction
regression: `安全性改修は必要なので、ここでは受け入れて次に進もう。次はなに？`
`segment-implementation/safety-cost-acceptance/acceptance.json` records the decision and
`verification.json` binds the three document updates with exact source/prior-evidence preservation.

This is an explicit safety/performance trade-off, not a proof that every residual instruction is necessary.
It applies to the retained0386 encoded-image implementation after the measured corrections, not
0392 decoded staging, the single-work-image trial or other rejected alternatives. No further residual
optimization investigation is requested. The earlier HOLD labels and failed/incomplete receipts
below describe their historical checkpoints; they are not rewritten as successful measurements.

The decision does not waive Clippy/workspace formatting, independent review, source-binding gaps
in diagnostic0388 or future W2 runtime/acceptance obligations. The acceptance checkpoint changed
only documents and supplied no validation/benchmark pass. The separate0393 closure below now
completes the actual review/lint/format gates; it changes no domain/API/layout or saved result.

The next functional boundary is the existing tiny promoter/leaf-pin geometry owner: prepare one
combined same-leaf placement/count image and complete all fallible admission before publishing
mate shrink, physical start, vertex extent or PMA counts. This remains planned, not implemented.
Queue-owned reservation/admission and Graph first-write gates follow; live width delegation/common
inline+step publication comes after those prerequisites, then the unchanged0383 F2/F3/F4 fixtures.
0393 closes review/lint/format separately, without reopening performance investigation.

## Planned tiny/leaf geometry (0394)

**Status: Planned;0396 mode prerequisite is closed, but0394 requires a fresh baseline. Q1 remains draft-boundary approval only.**
[Plan0394](../../plans/0394-grouped-tiny-leaf-geometry.md) scopes one same-leaf tiny-promotion group
or first-pin publication unit. It reuses the existing physical-residency, descriptor, vertex,
slab, span-meta/count and allocator owners. Scalar promotion becomes a singleton of that unit;
relocation retries, complete insert/materialize operations and paired orientations are not its
atomicity boundary. The approved0384 packet's §§1–3,5–6 remain the contract, not a new width API.

The source audit found pin/mate writes preceding fallible descriptor/count work, duplicate
content-end calculations using logical tree width, and an earlier-label promotion that can change
the inferred first-non-tiny cover anchor without moving old residents.
[GAP-LARA-TINY-GEOMETRY](../implementation-gaps.md#gap-lara-tiny-geometry--tiny-promotion-publishes-before-complete-geometry-admission)
records the inspected owners, native reproduction and prerequisite disposition. After the user's
`進めて`, three owning cfg(test) proofs were added before any production geometry edit. Each exact
invocation ran1 test. Pin/count returns exact SegmentCountsTreeTooSmall but already changed logical
capacity256→272 (`tiny-leaf-red-pin-count.log`, exit101,15.93s). Singleton earlier-label promotion
returns Ok, not the required no-relayout rejection (`tiny-leaf-red-earlier-anchor.log`, exit101,1.04s);
subsequent preservation assertions are not reached in either RED.

The corrected current-caller gate completes all eight cases: real insert, inline materialize,
stepped materialize and width-on-slot at compatible width0, in both promotion orders. Exact target
sequences/schema/global/leaf counts pass, then all four later-first cases fail final containment:
cover[260,269), residents[260,264) and[256,260). `tiny-leaf-stop-current-callers-width0.log` exits101
in6.89s. The initial width2 attempt instead fails intentional nonempty width admission and is
preserved as a fixture correction, not stop evidence or a new width-success contract. These native
assertion failures are reached typed outcomes/postconditions, not fabricated operation-error seams.
No data-loss, valid-reopen or benchmark result is claimed from this proof.

0394's mandatory STOP therefore applies. [Plan0395](../../plans/0395-pinned-vertex-physical-layout-preparation.md)
uses `compact.rs`'s existing resident snapshot/position/commit owner for physical-copy admission.
Its r5 draft boundary is independently approved; the physical prerequisite now has native715,
completed I2 implementation approval, explicit user cost acceptance and final closure evidence below. A scalar guard would reject required success,
and a sequential publishing rewrite would not be the final combined unit. Do not change the cover
rule, add an unsafe fallback or mask the defect with64 retries.0394 must be re-scoped/reviewed for
copy composition before resuming; queue/Graph/live width/0383 remain later gates.

`segment-implementation/tiny-leaf-geometry/implementation-baseline.json` and `implementation-before/`
freeze the dirty base and archive the retained0386 Wasm. `red-stop.json`, both `red-proofs*.rs`
revisions and `red-proofs.patch` bind the diagnostic source/transcripts. The temporary failing test
suffix is restored out of the repository byte-for-byte, not ignored or weakened; production source,
original708 test identities,0393 closure, approved packet, index/lock/Wasm/saved results remain
unchanged. One complete proof pass uses10 native graphs/160 memories; including the initial
width2 attempt, actual execution used17 graphs/272 memories,0 PocketIC/installs/new E2E binaries.
No GREEN/mutation/new-implementation Clippy, mixed-mode/group runtime, performance or0383 acceptance
follows from the stop. The accepted0386 cost is not reopened or extended.

Fresh restored-baseline native708 and workspace formatting pass (exit0,24.82s and24.56s;
`tiny-leaf-stop-restored-native.log`, `tiny-leaf-stop-workspace-format-check.log`), not a green fix
of the archived defects. Both original draft structural validators pass. Preserve0395's incomplete
r1/r2 attempts (exit124/300.10s and300.03s) and their input/diagnostic bindings; neither has a verdict.
The duplicated read protocol was replaced with one complete coordinator-verified inline packet,
without modifying source or global model/cache/auth configuration. Completed independent r3 has
ZERO tool actions, exit0/206.00s and `NOT APPROVE PREREQUISITE DRAFT BOUNDARY`; its receipt and
required revision queue are `prerequisite-review-r3-{completion,revision-queue}.json` under W.
The reviewer supports0394's STOP but requires explicit non-publishable versus physical-only ready
capabilities, one vertex/selected-leaf unit with sole count/retirement ownership, and stronger
branch/error/CsrEdge-only assertions.0395 now specifies those planned corrections and adds
`edge/span.rs` plus the bounded exact-prefix preparation/shared split projection in
`edge/free_span.rs` to its future source manifest. Source inspection before r4 found that the
existing guarded replacement API chooses best-fit, while in-place expansion consumes a specific
adjacent prefix. The planned owning extension preserves that policy and shares existing readiness;
no new allocator/index/layout is proposed. The gap/source hashes are recorded in
`W/prerequisite-r4-prefix-admission-gap.json`. Completed r4 (exit0/164.52s, ZERO tools) still ends
NOT APPROVE: the ready capability/slide/original assertions are satisfied in the draft, but the
source-cover retirement owner and free-hole success assertions remain required. Its immutable
completion/revision queue are under `prerequisite-review-r4-{completion,revision-queue}.json`.
0395 now specifies one private `FreeSpanStore::prepare_physical_replacement` for guarded canonical
free-hole subtraction, selected-prefix/coalescer admission and kept-source ready validation,
with successful fixed-interval best-fit/append cases and allocator publish1/release probes0.
The separate targeted-prefix proposal is superseded; only the existing owner/source-reader
plumbing is extended. At this draft-review checkpoint no source or stable layout changed.
Completed focused r5 (exit0/116.36s,
ZERO tools) ends **APPROVE PREREQUISITE DRAFT BOUNDARY**; its write-once
`prerequisite-review-r5-completion.json` binds the exact packet/diff/completed turn and classifies
all required corrections as satisfied by DRAFT contract. Earlier NOT APPROVE evidence is retained.
The subsequent user request allocates0395 implementation. Its18-path baseline/manifest and
retained Wasm are frozen before source edits. The first BEFORE install fails on export-name
size20125>20000; the two explicit short measurement-only queries correct this to19997 without
changing original exports/fixture semantics. Corrected verification and all three BEFORE controls
now pass, bound by `W/physical-layout-implementation/before-{export-verification,benchmark-results}.json`.
[GAP-LARA-BENCH-EXPORT-NAMES](../implementation-gaps.md#gap-lara-bench-export-names--aggregate-benchmark-method-names-exhaust-the-install-limit)
retains exact dependency/Wasm evidence and the unresolved broader export capacity.

**Intermediate allocator checkpoint (superseded by the physical graph checkpoint below):**
0395 had reached allocator preparation; enclosing physical graph publication was still pending. The single
`FreeSpanStore::prepare_physical_replacement` owns guarded original-free subtraction, shared
prefix/coalescer admission and source-bound ready validation. Its owning test proves fixed free
coverage/statistics, exact-prefix versus a smaller best-fit competitor, ready/drop preservation,
publisher1/release probes0 and changed-hole/neighbor rejection before publication. Native709
(allocator34/bucket17 retained) and strict lib/test Clippy pass. Seven fresh stores/fourteen
memories, new graphs0/PocketIC0/installs0. This was only the allocator portion of one planned test.
At this intermediate checkpoint, six identities and the enclosing graph assertions were still
pending; native709 was not native715 or0395 closure.
`W/physical-layout-implementation/allocator-checkpoint-results.json` binds current producers,
archived Wasm, CSVs and terminal results. Intermediate slab/tree costs remain exactly
1,826,569/1,842,268 instructions with growth0. Prefix cost1,027,928→1,047,250 (+1.8797%) is reduced
from approximately3.7% by compact source-fact scratch and out-of-line physical validation; this
is explicit new-diff cost, not assumed noise or inherited0386 tolerance. Final publisher/cost
admission, overlap/late-error mutation proofs, independent implementation review and final plan
validation remain pending.0394 stays STOPPED; width/0383 and outer-operation guarantees remain
unproved. Index/lock/saved results and old evidence are preserved; no stage/commit/push.

The subsequent buffered slab checkpoint replaces the single-range constructor with
`EdgeSlabStore::prepare_physical_image`, shared by both existing descriptor-store uses. All
buffered ranges/backing are admitted before payload writes; one encoded header follows all ranges.
Its partial backing-rejection identity reaches exact real grow failure, pre-grow later-range
rejection, reserve/drop preservation and a bounded premature-write counterexample, with a
CsrEdge-only edge type. One fresh slab store/one memory, graphs0/PocketIC0/installs0. Native710,
allocator34/bucket17, strict Clippy and formatting pass. The five unwritten identities, descriptor
encoding rejection, enclosing graph snapshots/events and canonical ready/consume remain pending.
`W/physical-layout-implementation/slab-image-checkpoint-results.json` binds producers, restoration,
archived Wasm and exact intermediate controls1,826,569/1,842,268/1,047,250, growth0. The measured
physical graph writers are still unchanged: no new graph-admission cost, final AFTER or independent
implementation approval is claimed. Read-only planning agents could not review because of account
model/login errors; no verdict substitutes for the final gate. Runtime ledger recovery is separately
bound in `parallel-ledger-recovery.json`; immutable evidence is preserved and calls are serialized.

### Physical graph publisher checkpoint (0395; historical checkpoints)

The r4/publisher pending labels below describe those earlier checkpoints, not the current disposition.
The separate user acceptance and final closure are recorded after I2; old receipts stay immutable.

`PreparedVertexPhysicalCopy` reserves/revalidates desired logical shapes but has no publication or
ready-construction capability. Only `prepare_physical_layout(VertexRewrite|LeafSlide)` constructs
the canonical physical-only unit. It captures the selected leaf before any write, prepares all
resident ranges, existing descriptor slots, encoded vertex rows, sole count projection, moved pin
and guarded allocator image, then revalidates immediately before ready. Disjoint relocation retains
fixed raw source ranges/chunk scratch; overlap buffers every source. Ordinary rewrite and both slide
branches consume one `ReadyPhysicalLayout::publish`; obsolete materialization/commit finalizers,
post-slide release/count reconciliation and per-vertex slide deltas are removed. One global source
snapshot replaces repeated per-participant captures; desired-shape construction still revalidates
its own component. No tiny/width/outer-operation atomicity is claimed.

All seven contract identities now exist. Native715/0, allocator34/bucket17, strict lib/test Clippy,
workspace format and whitespace checks pass. Real late descriptor/backing rejection preserves
canonical unit bytes, allocator and warmed caches with publisher0. A bounded wrong implementation
writes the first overlapping payload before later admission: the unchanged-canonical-bytes assertion
fails; mutationr4 is fully restored byte-exact and its owning exact test passes. These are native
returned-error boundaries, not rollback after arbitrary traps.

Independent contract T1/T2 NOT APPROVE findings are repaired only in the existing tests: exact
real-consumer retirement/publish/Pin checks, live mixed tree/property roots in both rejected paths,
and shared literal free coverage/statistics asserted live and after all three free-hole reopens.
Completed T3 (exit0,54.41s,ZERO tools) ends **APPROVE CONTRACT PROOFS ONLY**; its exact report/input/
completion are `W/physical-layout-implementation/physical-contract-review-t3-*`. Current r4 native715,
all seven exact identities, strict Clippy, workspace format/whitespace and canbench-feature Wasm
build pass at that r4 checkpoint. Its production was byte-identical to the publisher checkpoint;
graph/memory budget23/368, reopens12, no new fixture identity or PocketIC constructor for those repairs.
Whole production/API/design
implementation approval remains a separate gate, not implied by this contract verdict.

Frozen64 AFTER instructions are slab1,530,362/tree1,577,466 (BEFORE1,826,569/1,842,268), heap/stable
growth0. These final producers include typed fallible reservations at the existing source loader,
position calculators and retirement planner; their +27,393 instructions versus the prior AFTER
is an explicit admission-safety cost, while both controls still improve versus BEFORE. Native replay of those geometries measures boundary reads/writes, retained resident bytes
1536/2048 and streaming scratch0; it is an explicit alternate observable, not Wasm system-call tracing
or peak-heap measurement. The first prefix result1,054,418 exposed a new shared-constructor cost.
Trace-guided inlining of the bounded scratch admission helper reduces it to1,048,658, still
+20,730/+2.0167% versus BEFORE and +1408 versus the allocator checkpoint. The remaining cost is
explicit and unapproved;0386's accepted regression is not extended. Existing controls and failed
first measurements remain immutable. The source/Wasm/CSV/terminal/restoration/preservation bindings
are in `W/physical-layout-implementation/physical-publisher-checkpoint-results.json`.
Fresh source-bound r4 build/control receipts, `after-physical-contract-r4-binding.json` and three
immutable CSVs reproduce all three instruction/growth values after the test-only proof corrections;
old binary/CSV bindings are retained unchanged.

T3 itself did not provide production implementation approval. Completed I2 now supplies APPROVE
IMPLEMENTATION ONLY as recorded below. I1's complete frozen-base source/active-doc packet timed out
at300.04s, exit124,
with ZERO tools and no assistant/turn completion. Its immutable incomplete receipt and streamed
text are diagnostic only, not a verdict or cost disposition. At that checkpoint, source inspection
identified unpinned-overlap live-retirement and infallible singleton/stable-sort admission corrections;
`physical-implementation-i1-required-corrections.json` retains those exact owning sites.

I2's existing overlap identity reaches the new runtime red: real unpinned100→98 rewrite frees `(100,8)`
versus required `(106,2)`, exit101. Compact now subtracts the whole final cover, including slack,
from the old candidate intervals before the allocator's unchanged source-free subtraction. Both
100→98/102 directions reuse the original graphs and assert exact freed2slots, publish/count1, Pin0,
cache/run/peer/payload preservation, native reopen and overwrite of only the reallocated retired slots.
Current-block/adjacent retirement0 and best-fit/append original-free/disjoint ownership remain unchanged.
The singleton position vector reserves fallibly; allocation-free unstable participant sorting preserves
descending anchors and canonical ordinal ties. No new helper, module, API, schema or duplicate writer.

Native715/all708/allocator34/bucket17, all seven exact identities, strict Clippy, workspace format/
whitespace and canbench-feature Wasm build pass. Graphs/memories stay23/368; two extra durable-retirement
reopens change12→14 with no new fixture identity/PocketIC constructor. I2 source-bound controls are
slab1,531,031/tree1,578,135/prefix1,048,658, growth0, and frozen fixture source is unchanged. The669
increase in each physical control versus r4 is explicit, without instruction-by-instruction attribution;
both remain below BEFORE. Prefix +20,730/+2.0167% was still unapproved at I2 review, not waived
by safety wording; its subsequent explicit user acceptance is recorded below.
`after-physical-implementation-i2-binding.json` and fresh immutable CSVs retain the current producers;
all earlier binaries/CSVs and I1/T3 inputs remain unchanged.

Completed independent I2 (exit0,205.65s,ZERO tools) confirms all three mandatory corrections and all
seven current contracts without actionable implementation/document P1/P2; it ends **APPROVE
IMPLEMENTATION ONLY**, separately **COST DISPOSITION NEEDS USER DECISION**. Exact full frozen-base
Rust/active-doc input, report and turn receipt are `physical-implementation-review-i2-*`. Necessary
fallible metadata/common-owner semantics do not establish that all residual immediate-prefix20,730
instructions are indispensable. That review required an explicit user decision; earlier `次に進めて`
was not cost acceptance. Subsequent status updates change approval evidence only, not reviewed
source/execution contracts. Earlier model/login errors supplied no verdict; no stage/commit/push
or saved-result update occurred.

### Explicit0395 cost acceptance and closure

The user asks `必要そうなコスト？`, then accepts with `そうしよう` after the recommendation to retain
the shared implementation/safety contract and accept the known approximately2% overhead without
calling it a proven minimum. `W/physical-layout-implementation/physical-layout-user-cost-acceptance-u1.json`
binds this decision to BEFORE1,027,928/AFTER1,048,658, +20,730/+2.0167%, allocator-checkpoint delta1408
and unchanged I2 source/Wasm/CSV/review producers. This is **ACCEPTED WITH KNOWN REGRESSION BY USER**,
not independent attribution of every instruction, a noise claim, inherited0386 tolerance or acceptance
of future0394 costs. No validation gate is waived or residual optimization authorized.

The completed I2 native715/all7 exact, strict Clippy, format/whitespace, canbench-feature Wasm and
three frozen controls remain source-bound; no Rust source changes follow that review. Closure updates
only evidence/status documents, so those Rust/build/benchmark commands are not redundantly rerun.
`vertex-layout-plan-final-u1.log` records the exact final plan validator; the write-once
`physical-layout-final-closure-u1.json` binds final source/doc/terminal/preservation facts, including
all changes within the18-path manifest and unchanged protected evidence, saved results, HEAD/index/lock.
0395 closes only its physical-only prerequisite.0394 remains STOPPED with the original archived REDs;
its separately reviewed copy-composition scope/current-caller gate must precede integration. Tiny
geometry, width, queue/Graph, outer operations and0383 remain unproved. No stage/commit/push.

The first independent plan review required stronger success/count/cache assertions.0394 now
requires successful forced mate-tail splitting in both orders, independently fixed same-vertex
payloads/extents/aggregate counts and warmed cache fields checked before any refill. Focused
read-only re-review ends **APPROVE DRAFT BOUNDARY**, exit0,117.05s; the completed turn and20 inspected
source/plan/document/contract hashes are bound in
`segment-implementation/tiny-leaf-geometry/review-r2-completion.json`. The first NOT APPROVE and
its correction queue remain preserved. All implementation TODOs/criteria stay pending. That earlier review approved only the draft
boundary; the subsequent user allocation and failed native stop proofs above do not close it.

### Post0395 copy-composition draft revision (0394 Q1)

The user's next `進めて` authorizes plan/document rescope and independent review only. The new
`W/geometry-copy-composition/rescope-q1-baseline.json` freezes current post0395 Rust/plan/closure/
protected evidence and the exact four-path revision surface.0395's original18-path closure and
user-cost decision remain immutable as-of closure. No Rust implementation or validation command
is authorized by this revision, and the old stopped0394 baseline is not reused for production edits.

The draft replaces unsupported append-only/no-relayout rejection with required earlier-label
copy-composed success under the existing first-non-tiny cover rule. `PreparedVertexPhysicalCopy`
stays nonpublishable; same-leaf geometry validates logical Tiny→Slab/mate changes, while the existing
compact preparation/consuming owner admits all desired copy/descriptor/vertex components with one
final header/pin/count/allocator image. Canonical physical-only factories retain their restrictions;
no ordinary rewrite commits as preparation, callback, second writer loop or unsealed desired-ready
conversion is planned. Buffered retained slab-prefix/combined-root bytes are real scratch, not
bounded by four new targets per promotion. Values' three callers and generic wider-edge first pin
remain supported without enabling live width or outer atomicity.

Native baseline715 retains all seven0395 identities; planned13 geometry functions imply728. The
old three REDs, exact failed assertions and prior draft approvals keep their historical contracts.
The former singleton append-only rejection identity is superseded, not relabeled green; all eight
real caller/order successes still need stronger containment/payload/reopen GREEN. Planned geometry
fixtures32 graphs/512 memories/reopens16 plus unchanged physical23/368/14 are frozen from actual
written source before execution; PocketIC/installs/new E2E0.

The misleading `tiny_promote_d3_1024` fourth-inline-insert benchmark is intentionally superseded
by same-length `tiny_promote_d4_1024`, seeding four and measuring the fifth. Correct fixture/export/
install sanity and new BEFORE must precede geometry edits; previous measurements remain archived.
No new benchmark method, generic export split or unrelated control removal is implied by the
current3-byte method-name headroom. Rerun retained0395 physical controls for the shared publisher;
neither0395's +2.0167% acceptance nor0386's acceptance covers a new regression. Every implementation
TODO/criterion, runtime/cost/implementation review gate and later queue/Graph/width/0383 slice remains
pending. Completed independent Q1 review exits0/101.35s, one turn, ZERO tool actions, with no
required P0/P1/P2 and APPROVE DRAFT BOUNDARY; `geometry-copy-composition/rescope-review-q1-{input-binding.json,report.md,completion.json}`
binds reviewed sources/status changes and completed verdict. Draft structural/whitespace/exact
four-path preservation checks pass; final approval-status checks preserve all Rust/0395 plan/closure/
old RED/index/lock/saved artifacts. The original reviewed packet remains immutable. This approves
only the draft composition boundary; separate production allocation and every implementation/runtime/
performance/0383 gate remain required. No Rust command or code edit follows this review.

### Mode footprint prerequisite draft P1

[Plan0396](../../plans/0396-mode-footprint-handoff.md) has r3 independent draft-boundary approval:
one completed assistant turn, zero tools, no remaining concrete P1/P2. It requires checked count
backing before source reads, explicit zero-live prefix/run ownership, an allocator-driven tree
capacity-growth witness and one Failure scope separating ordinary byte preservation from exact
unpublished tree-target cleanup. No production repair, runtime/performance approval or0396 closure
is implied;0394 remains stopped and0395's closure is unchanged.

One new partial test, `mode_footprint_retained_roundtrip_preserves_assigned_pin`, is frozen before
RED with one outer constructor/33 memories,0 reopen/PocketIC/install. Exact1 fails, exit101/10.91s:
totals11→80→11 versus assigned16 throughout, normal cover69 versus physical5. Exact69 rows in
ascending/descending order, prefix5/run64/actual69, edge header and free spans pass first. The test
remains deliberately RED; the old715 identities pass with only that identity excluded. Scoped
lib/tests Clippy and workspace formatting pass, not all-tests GREEN. Remaining seven identities,
empty/mixed/unassigned/reopen/backing/tree/mutation/BEFORE/AFTER and implementation review are pending.

`W/geometry-copy-composition/mode-footprint-p1/{entry-manifest,first-fixture}.json`,
`first-fixture-{source.rs,patch}` and `draft-review-r3-{report.md,completion.json}` bind this partial
entry. The unbound review startup's403 errors and failed fixture formatter-spelling check are
recorded tooling failures, not reviews or runtime contracts; the source/read-order review was
completed through explicit provider/model binding, and the fixture check normalizes Rust whitespace.
No production bytes or saved benchmark results change in this phase; all protected receipts retain
original hashes. This remains a test-first checkpoint, not implementation closure.

### Historical mode prerequisite P2 locked preflight stop

At P2, continuation0396 froze the P1 checkpoint, then failed the locked BEFORE Wasm build before
compilation (exit101/1.08s); locked target metadata also fails (exit101/1.29s). A separate offline
metadata mirror resolves a candidate1227→1083-package lock, exposing morph-dict's missing
unicode-normalization dependency and obsolete/disambiguated explorer/GPUI rows. No changed
canbench/ic/LARA package rows are observed. A separate candidate with only the morph-dict row
updated still fails locked metadata (exit101/1.35s). These are diagnostic observations, not runtime,
performance or lock-repair approval; the real lock retains its original hash and all Rust is unchanged.

[The workspace lock gap](../implementation-gaps.md#gap-workspace-lock-drift--locked-build-prerequisite-resolved)
required separate authorization/review outside0396's then-owned surface. Fresh BEFORE and further
source implementation were blocked at P2. Do not remove `--locked`, silently
refresh canonical dependencies or substitute the retained old Wasm/results. The existing partial
RED is retained, all0396 criteria stay incomplete and0394 remains stopped. P1's715-pass/Clippy/format
results are historical, not re-run P2 gates. `W/geometry-copy-composition/mode-footprint-p2/{baseline,
lock-diagnostic,lock-delta-summary,minimal-lock-diagnostic}.json` and the bound terminal logs preserve
source, diagnostics and stop scope. No stage/commit/push occurred.

### Mode prerequisite P3 lock alignment and fresh BEFORE

User `いいよ` separately authorizes lock-only alignment. The bounded r3 packet-only review approves
its boundary (exit0/13.50s, one completed turn/ZERO tools); prior inventory/count objections are
resolved by exact deterministic/indexed evidence, without changing the candidate or old reports.
Real Cargo offline metadata reproduces the exact reviewed lock SHA256
`f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.
The complete delta is144 removals/zero additions; retained versions/sources/checksums and LARA's
optional-inclusive168-package closure are equal. Six obsolete optional-edge reductions and the
current morph-dict manifest edge are explicit. No source/manifest/version upgrade is authorized.

Locked target metadata passes (5.07s), and the original locked Wasm build passes (128.76s).
Fresh BEFORE instruction controls pass: `vls64`1531031, `vlt64`1578135,
`tiny_promote_d4_1024`190825723, `bench_l_def_bp_iter_128`18854; each0 heap/stable growth pages.
Export-name bytes remain19997/20000. `W/geometry-copy-composition/mode-footprint-p3-lock/
{before-source,before-wasm,before-results}.json` bind current source/lock/fresh binary/CSV commands.
External evidence-only configs measure that frozen newly compiled binary; no repository benchmark
results are persisted or historical measurements relabeled.

The old runner's125 original-lock guard remains failed, not a successful application command.
Its current0396 successor uses only the reviewed real lock, deleting executing old-lock borrowing;
original runners/receipts remain immutable. A separate archive receipt-key failure is retained and
its already-frozen outputs verified before completion. All Rust/manifests/index/protected evidence
remain unchanged. Native715 original identities pass under the current lock (8.95s) only with the
reproduced deliberate RED excluded; exact1 again reaches totals11→80→11/normal cover69 (1.52s).
No all-tests GREEN, production mode/count/range/tree repair, AFTER, mutation, implementation review
or0396 closure is claimed. All0396 TODOs/criteria stay pending,0394 stays stopped,0395 is not reopened.
No stage/commit/push occurred.

### Mode footprint prerequisite P4 native implementation

[0396](../../plans/0396-mode-footprint-handoff.md) implements the admitted final bypass row directly
through `SegmentRewritePlan`; descriptor retirement, vertex encoding and checked count/source
admission precede ordinary publication. The former empty normal intermediate and late fallible bump
are removed. Compact/ordinary/tree count selection and assigned-range interpretation live at the
existing EdgeStore/count owners. Tree allocation keeps its separate owned-target cleanup contract
and exact post-allocation header witness; it is not a pin resize. The
[storage contract](../storage/lara.md#3-segment-physical-contract-rope) describes current behavior.

All723 native identities (715 old plus8 new), strict lib/tests Clippy, format, three exact restored
mutation witnesses and locked Wasm compilation pass. `W/geometry-copy-composition/mode-footprint-p4/
{fixtures-r4,native-validated}.json` binds290 native memories/8 reopens and the source/test inventory.
The zero-live prefix expectation now preserves its default descriptor and tombstones; the relocation
fixture materializes a real prefix without weakening exact67 rows, both orders or sidecars. No saved
benchmark results, protected0395 evidence, MemoryId or persisted layout is changed.

Independent packet-only review returns REQUEST CHANGES (148.22s, one completed turn/ZERO tools).
It identifies the late tree callback reservation (P1) and missing exact LTB cleanup/reopen observability
(P2). `mode-footprint-p4/review-{report.md,completion.json}` binds that original candidate and result.
P5 user-authorized continuation deletes both fallible callbacks and separates existing normal-image/
bypass-segment preparation/ready from one final transcriber witness admission. The same tree fixture
moves count injection to descriptor reserve, preserves active LTB0, checks exact free targets1/2/links
and source bytes, and verifies exact logical rows/allocator integrity on its existing rejection reopen.
The bypass layout audit uses initialized prefix, not logical spill-inclusive width. Budget stays290/8.
Native723, strict lint/format and late-admission mutant/restored GREEN pass.

The explicitly approved fresh current runner keeps the old real-lock execution contract immutable
and admits four new evidence-only AFTER directories. Fresh locked Wasm/export inventory and CSV
controls pass: slab1530725/tree1577829 (each-306), tiny191032946 (+207223/+0.1086%), bypass18854 (0),
all page growth0. Independent re-review accepts the bounded tiny safety-admission cost without
weakening sanity, persisting partial results or borrowing0395 approval. `mode-footprint-p5/{runner-permission,
fixture-red-r2,native-validated,mutation-restored,after-results}.json` binds this continuation.
Packet-only re-review returns APPROVE IMPLEMENTATION AND COST BOUNDARY (45.80s, one completed
turn/ZERO tools), no remaining concrete P1/P2. `mode-footprint-p5/{review-completion,pre-closure,
closure}.json` binds the unchanged reviewed code, all required gates and final real-plan/source/
protection/index preservation at the pre-commit checkpoint. The subsequent user-authorized scoped
commit and index-only validation are bound by `mode-footprint-user-commit-c1/commit-receipt.json`;
no push is authorized.0394 geometry still requires a fresh723-native baseline. No0395 or original
BEFORE/RED binding is rewritten.

## Validation closure (0393)

[Plan0393](../../plans/0393-lara-validation-closure.md) repairs the recorded41 test diagnostics across
13 exact LARA paths and formats the22 exact workspace paths listed in
`segment-implementation/validation-closure/baseline.json`. Meaningful logic edits stay cfg(test);
other files are formatting-only. Eleven ignored traversal results now assert exact Continue.
Existing assertions, source IDs,4096-prefix-plus-distinct-tail coverage, fixtures and test inventory
are preserved. No helper/module/dependency, domain behavior or functional geometry change was added.

| Current gate | Terminal evidence |
| --- | --- |
| `cargo clippy -p ic-stable-lara --locked --lib --tests -- -D warnings` | PASS, exit0,5.85s; `segment-validation-clippy.log` |
| `cargo test -p ic-stable-lara --locked --lib` | PASS708/0, allocator34/bucket17, unchanged identities;22.03s; `segment-validation-native.log` |
| `cargo fmt --all -- --check` | PASS, exit0,3.50s; `segment-validation-workspace-format.log` |
| Independent read-only review | APPROVE RETAINED TEST and APPROVE DIAGNOSTIC CLEANUP; exit0,249.73s; `segment-validation-independent-review-catalog.log` and `validation-closure/independent-review-catalog.md` |
| Final0393/0386 structural checks | PASS; `segment-validation-plan-0393-final.log`, `segment-validation-plan-0386-final.log` |
| Changed-line whitespace/preservation | PASS; `segment-validation-whitespace-final.log`, `validation-closure/verification.json` |

Within `segment-implementation/validation-closure/`, `validation.json` and `review-source-binding.json`
bind the exact35 reviewed source hashes; `scoped-review.patch` is the canonical frozen-dirty-base
Git diff. `source-verification.json` preserves those inputs before document closure, and
`verification.json` binds final documents, all unrelated bytes, index/lock/retained target and old
evidence. The explicitly new ignored0393 plan is the sole path addition. Earlier source checkpoints
and failed/incomplete transcripts remain exact, not retrospectively reclassified as passes.

The retained `prepared_exact_prior_checks_late_edits` file is unchanged. Independent review verifies
that comparing only the first3 priors rejects position3 but incorrectly admits position4, where the
archived mutant fails. Distinct IDs, exact split/retirement results/statistics/reopen and exact
BinInvariant with reached byte/cache preservation are covered; arbitrary positions and scratch OOM
are not. The reviewer ran no tests/builds/benchmarks/delegation. Unsupported-model startup attempts
and a corrected read-loop PATH failure remain separate failed inspection receipts, not approval.

No benchmark rerun or current-source Wasm equivalence is claimed: production behavior is unchanged,
the retained target is merely preserved, and accepted+3.8–6.4% costs retain their original bindings.
Full-workspace runtime/PocketIC/0383 acceptance, diagnostic0388 criteria2/7, scratch-OOM injection
prerequisites and later tiny/leaf/queue/Graph/width implementation remain outside this closure.
No stage, commit or push occurred.

## Evidence binding

External evidence root:
`/Users/yota/.local/share/gleaph/evidence/adr0097/maintenance-fixtures-f2-f4-0383`.

`segment-implementation/baseline.json`, `before/`, `dirty-before.patch` and `ledger-before.json`
bind the pre-existing dirty state. Comparisons use those bytes, not HEAD alone.
`owned-manifest-v2.json` enumerates nine Rust paths, the active storage document, this report and
the ignored plan. The required slab/vertex facade additions have frozen before-images.

The approved `width-owner/contract-v2.md` SHA256 remains
`b08330854aa683a75da43815b7953315431e33a173f748e6263e54535cb2616a`.
The user authorized the additional 600 seconds. `budget-extension/authorization.json` binds the
1310→1910 ceiling; `source-sampling-extension/receipt.json` binds the additive writer-source
sampling. Execution logic and the continuing ledger were preserved. Prior plans, approval
artifacts, 0383 fixtures and earlier evidence remain protected.

The original `checkpoint.json`, `owned.patch` and `native-red-source.rs` remain historical
preflight evidence. Their production-unchanged/test-only assertions apply to that checkpoint.
Implementation preservation uses additive v2/v3 checkpoints; it does not rerun or rewrite the
preflight receipt. The v2 implementation snapshot predates the source-footprint correction below.
The v3 binding remains historical. `review-fixes/authorization.json` records the second approved
600-second extension,1910→2510, without a ledger reset. Its frozen `before/`, ledger prefix and
`prior-segment-evidence.json` protect the previously reviewed source and67 segment artifacts.
`review-fixes/closure.json` binds corrected tests, all nine final Rust hashes, diagnostic mapping
and measurements; `checkpoint-v4-final.json` records that source/document preservation binding.
The subsequent residual investigation is authorized by `residual-cost/authorization.json`,2510→3110
with a bounded additional600 seconds. `checkpoint-v5-final.json` binds its final documents, rejected
controls and restoration; v4 and all earlier artifacts remain unchanged.
`prior-values-inline/authorization.json` records the accepted next600 seconds,3110→3710, and freezes
v5 with its current source/document hashes and ledger prefix. `prior-values-inline/closure.json`
binds the rejected control, new native test, restored production and fresh validation. The additive
`checkpoint-v6-final.json` binds the resulting documents and prior-evidence preservation.

## Publication owner

The original normal publisher wrote replacement descriptors and vertex links before fallible
old-span release. The native RED run reached retirement record grow 3, returned
`RebalanceFailed(GrowFailed { current_size: 0, delta: 0 })`, and showed all three links already
moved. Its later source/header/reopen assertions were not reached. The same test now passes
without weakened assertions.

The symmetric callers audited were normal compact, clear, insertion without slack and removal,
plus `graph/bypass.rs::promote_bypass_to_bucket_mode` and
`graph/promote.rs::promote_bypass_with_edge`. `rewrite_segment_bucket_rows`,
`BypassPromotionPlan` and the separate bypass reserve/commit functions were deleted. The normal
insertion slow path prepares its return locator before publication. In-place insertion retains
its existing no-relocation guard and complete-old-row validation.

The closing audit found a source-footprint gap: live descriptors could fit the logical slab while
slack crossed `elem_capacity`, so old-span retirement could overlap an appended destination.
`segment-owner-source-footprint-red-result.log` records the native operation returning `Ok(())`
and the new exact-rejection assertion failing (0/1); following preservation assertions were not
reached in RED. `label_bucket_descriptor_span` now owns the checked live-plus-slack capacity rule,
shared by segment planning, retirement preparation and in-place insertion. The added case in the
existing admission test checks an untargeted peer and the fast path, exact errors and unchanged
state/reopen. No standalone test, alternate validation rule or layout was added.

`SegmentRewritePlan` captures the current owning segment once and combines desired target rows.
Repeated targets reject; two changes in one segment produce one final image. `SegmentSource`
revalidates owner/header, vertex rows and descriptors during reserve and ready admission.
`FreeSpanStore::prepare_replacement` keeps the actual selected whole free span, validates old
ranges against the original source, then prepares prefix consumption and coalesced retirement
using the existing allocator edit. Canonical allocation is tentative until publication. Discard
leaves the original free prefix or append accounting untouched, with no restore or suffix-free
mutation. A second preparation failure cannot undo another operation's spans.

The existing slab writer prepares checked offsets, backing, bytes and header;
`VertexStore::prepare_set` prepares checked existing-row encoding/address. Unsupported
`VertexAccess` implementations fail closed. Only native/canbench value fixtures explicitly opt
into value-only writes. `FreeSpanPreparation` retains the map guard separately from the borrowed
ready capability, avoiding unsafe or self-referential receipts.

`ReadySegmentRewrite::publish` writes slab bytes/header, prepared vertex links, then the admitted
allocator image. It returns `()`; no growth, encoding, allocation, overlap validation or returned
error follows the first write. Old descriptor ranges become free only after live links move.
Scalar and grouped allocator publication share `FreeSpanPreparation::ready` /
`PreparedFreeSpans::publish`. Cache installation uses prepared data and does not read the guarded
map. Fixture allocation also uses the same preparation and destination-accounting rule; obsolete
production allocation helpers were removed. Fixture release delegates directly to the allocator.

This guarantee covers descriptor segments. The existing outer bypass edge-span retirement,
counts/tree operations, grouped leaf/tiny geometry and value/Graph first-write gate remain outside
this slice. Public width declaration is still disabled.

## Native contracts

All 11 baseline owning test bodies remain byte-identical. Six tests were added, for 17 total:

| Test | Distinct contract |
| --- | --- |
| `segment_retirement_grow_rejection_is_unpublished` | Reached real record grow; exact error and original descriptor/vertex/allocator state, including reopen |
| `segment_group_same_segment_preserves_both_updates` | Both target orders and both preparation orders; both changes and distinct untouched peers survive |
| `segment_reservation_abort_preserves_actual_origin` | Actual best-fit reuse and append origins; second preparation failure and reverse discard preserve unrelated spans |
| `segment_ready_rejects_stale_source_and_invalid_rows` | Changed vertex/header/descriptor sources; source slack past logical capacity for peer compaction and in-place insertion; malformed fields/order/duplicate targets and unsupported accessor rejection |
| `segment_publication_success_paths_reopen` | Normal compact/insert/remove/clear and bypass publication; exact slack, edge/value/run references and real vertex-column reopen |
| `segment_bypass_rejection_is_unpublished` | Reached real by-start grow before bypass publication; exact source and paired allocator preservation |

The new scenarios use 24 native bucket-store fixtures and one real vertex-column fixture. PocketIC
constructors and canister/federation installs remain 0 before and after. Setup finds real grow
boundaries through owning operations; it does not truncate memory or fabricate a late error.
The corrected v4 runs passed owner17/allocator33/crate707 on the same nine Rust-file hashes.
All33 allocator test bodies remain byte-identical to the frozen0386 baseline. The later test-only
state below adds one allocator contract and freshly passes owner17/allocator34/crate708.

Independent review identified a self-referential expected descriptor image in the grouped test and
missing immediate peer checks after successful normal operations. The grouped expectation now
comes only from the fixture plus the explicit intended edits. `SegmentFixture::assert_rows` checks
all three complete vertex rows and descriptor vectors immediately after compact, insert, remove
and clear, both live and reopened. Both target orders and preparation orders remain covered.

For each finding, the reviewed test passed an injected wrong implementation. The strengthened test
then failed at its exact peer-descriptor assertion: grouped vertex2, success-path vertex1. Transcripts
are `segment-review-{group,peer}-{false-pass,mutant-red}.log`; the ledger and mutation snapshots bind
the inputs. Both mutants were removed before ordinary validation, and bucket-store production
bytes are identical to the previously reviewed implementation. Fixture and install costs are unchanged.

The focused independent read-only fix re-review completed in93.35 seconds:
`segment-review-fix-rereview.log`. It classifies the grouped expectation and immediate peer checks
CLOSED, allocator performance PARTIAL, and required-fix completion NOT APPROVE. No new correction
regression was found. It ran no validation; the native and benchmark results above remain the
execution evidence. The additive terminal receipts below were generated after that review.

## Measured costs

Baseline and final commands ran from `crates/ic-stable-lara` without `--persist`. The owner fixture
keeps 2 descriptors and 4 physical slots per vertex, compacted to 2 slots, at 2 and 32 vertices.
Separate-region exact-result prechecks run outside measurement. Fixture allocation moved to the
surviving preparation, outside measurement; initial logical geometry and the measured sequence
remain unchanged. All commands reused the shared target; no cold comparison was run.

These comparisons use this slice's baseline/final transcript totals. Canbench's displayed
"improved" percentages compare its older saved YAML and are not the 0386 comparison. Numbers and
percentages below are approximate at terminal display precision; heap/stable increases are
0 pages for every measured case.

| Case | Original0386 baseline | Reviewed v3 | Corrected final | Final vs original |
| --- | ---: | ---: | ---: | ---: |
| `bench_r_bs_segment_rw_2` | 88.46 K | 88.88 K | 86.52 K | −2.2% |
| `bench_r_bs_segment_rw_32` | 1.11 M | 887.07 K | 884.63 K | −20.3% |
| `bench_r_fs_tkbfs_256` | 974.46 K | 1.12 M | 1.03 M | +5.7% |
| `bench_r_fs_tkbfs_1024` | 973.47 K | 1.11 M | 1.01 M | +3.8% |
| `bench_r_fs_tkbfs_4096` | 1.00 M | 1.16 M | 1.06 M | +6.0% |
| `bench_r_fs_lbin_256` | 786.99 K | 926.90 K | 837.26 K | +6.4% |
| `bench_r_fs_lbin_4096` | 1.57 M | 1.73 M | 1.63 M | +3.8% |

The correction sequence isolates four changes inside the surviving allocator owner. Each uses the
unchanged fixtures and retains ready admission, exact prior-value checks, and index/cache maintenance.

| Scalar case | Reviewed v3 | Reuse encoded image | First candidate inline | Inline ready/publish | Inline prepare_edit |
| --- | ---: | ---: | ---: | ---: | ---: |
| `tkbfs_256` | 1.12 M | 1.09 M | 1.08 M | 1.05 M | 1.03 M |
| `tkbfs_1024` | 1.11 M | 1.08 M | 1.07 M | 1.04 M | 1.01 M |
| `tkbfs_4096` | 1.16 M | 1.12 M | 1.11 M | 1.08 M | 1.06 M |
| `lbin_256` | 926.90 K | 894.24 K | 888.08 K | 863.60 K | 837.26 K |
| `lbin_4096` | 1.73 M | 1.69 M | 1.68 M | 1.66 M | 1.63 M |

`FreeSpanEdit` reuses its encoded prospective record/bin buffers as the prepared write image instead
of allocating and encoding a second image. Its first heap candidate is inline; additional candidates
use the existing vector. `#[inline(always)]` on ready/publish and then `prepare_edit` measures the
call/optimization-boundary cost while preserving one publisher. The final attribute A/B grows the
uncompressed wasm from6,933,232 to6,941,727 bytes: +8,495 bytes, approximately+0.12%.

These controls remove approximately62–73% of the introduced scalar instruction overhead. Residual
+3.8–6.4% remains against the original0386 baseline. The shared admission and prior-value work is a
plausible contributor, but these aggregate measurements do not prove every remaining instruction
necessary. The earlier safety/ownership exception is withdrawn; performance disposition remains
HOLD until the residual is removed or supported by narrower causal evidence. No separate scalar
publisher, generic buffer framework, dependency or persisted layout was introduced.

`benchmark-baseline.json` and the original transcripts preserve the before-input binding:
`segment-owner-benchmark-baseline.log`, `segment-owner-allocator-split-baseline.log` and
`segment-owner-allocator-bin-baseline.log`. The v3 report and transcripts remain historical.
`review-fixes/measurements-closure.json` records all stages, exact source bindings and final deltas;
stage directories preserve their patches, hashes and wasm-artifact receipts. Final transcripts are
`segment-review-image-handoff-{benchmark,split,bin}.log`. No benchmark artifact was persisted.
The90-second encoded-split command timed out after compilation without a measurement; the
unchanged-input warm completion passed in6.51 seconds. Its incomplete result remains in the ledger.

## Residual-cost investigation (concluded at v5)

The fresh unchanged-source baseline exactly reproduces all five v4 scalar totals and is RED against
original0386: `segment-residual-feedback-baseline.log`. `residual-cost/feedback.py` records this
runnable regression check. The same fixtures, shared target, source sampling and runtime remain in
use; canbench and canbench-rs are0.7.0. No semantic, maintenance or admission step was disabled.

The additional256-case instruction trace used the already bound baseline wasm and existing runtime,
with integrity checking enabled. Its SVG/CSV artifacts are external under `residual-cost/trace-baseline/`;
no crate config or existing benchmark artifact changed. Canonical CSV total is1,027,928 instructions;
the separate traced run totals1,028,128. Constructor cost is78,063 inclusive instructions and paged-map
preparation500,429 inclusive. These nested totals are not additive and do not establish residual
necessity. The trace parser's first format mismatch was corrected against the actual `instructions`
label; its failed invocation remains recorded.

| Isolated control | Result against v4 | Disposition |
| --- | --- | --- |
| Inline `FreeSpanEdit::new` | Three displayed totals unchanged;4096 split −0.94%,256 largest-bin −0.29%; wasm +1,854 bytes | Weak reduction; all five residuals remain; attribute removed |
| Reserve release scratch for actual coalescing neighbors at one shared helper | One displayed total unchanged; four cases worsened +0.61–1.98%; wasm +539 bytes | Rejected; helper and changed reservation timing removed |
| Inline first three expected prior values with unbounded overflow and one paired append helper |130-second command ended during compilation; no benchmark or runtime result | Incomplete; representation/helper removed without a performance or correctness claim |

The constructor's first90-second command also ended during compilation; the same-input complete
command later took112.95 seconds. The scratch allocator's20-second command ended during compilation
with no tests run. All three incomplete commands and their exact source/artifact snapshots are
preserved. Compile-inclusive budgeting was corrected at the command-selection boundary; after the
next compilation timeout, long Rust work stopped. No cold target, alternate wrapper or delegated
validation was used.

All nine Rust files are restored byte-for-byte to v4; `segment-residual-source-restored.log` and
`residual-cost/restored.json` verify that fact and the matching existing17/33/707 runtime inputs.
Fresh scoped formatting passes. No control is retained, and no fresh runtime pass is claimed for
this investigation. The target wasm is a discarded control artifact and is not a final/current-source
artifact; existing benchmark results were never persisted or replaced. Earlier v4 runtime and owner
cost results remain source-bound to the exact restored code.

The bounded controls did not establish residual removal. They also do not prove removal impossible
or justify the safety/ownership exception. Image-handoff and prospective-codec hypotheses remain
unmeasured, and the inline-prior control lacks a completed build/result. Performance remains HOLD;
any later retained correction needs its own source-bound scalar/owner costs and runtime validation.

## Inline-prior control and retained test

Source inspection found a distinct expected-prior vector allocation in the v4 constructor, absent
from the original constructor. Each scalar closure performs16 take/restore pairs, so an inline
three-entry prefix removes32 allocations for that buffer. Original publication had separate encoded
buffers already removed in v4; this is not a claim about the net allocation count or proof that all
residual instructions come from this buffer. Expected values remain allocator-owned, while actual
values remain map-owned. `prior-values-inline/attribution.md` records this distinction.

The control first unified remove/insert's paired append at one helper, then kept an inline prefix
with unbounded overflow and used existing index-edit length as its sole logical length. It retained
all prior-value, source/backing, index/cache and returned-error admission. It completed native708 and
all seven unchanged canbench cases. No benchmark setup or maintenance was changed.

| Case | V4 | Inline prior | Control vs v4 |
| --- | ---: | ---: | ---: |
| `tkbfs_256` |1.03 M |1.02 M |−0.97% |
| `tkbfs_1024` |1.01 M |1.01 M |0% at displayed precision |
| `tkbfs_4096` |1.06 M |1.05 M |−0.94% |
| `lbin_256` |837.26 K |830.32 K |−0.83% |
| `lbin_4096` |1.63 M |1.62 M |−0.61% |
| `segment_rw_2` |86.52 K |87.68 K |+1.34% |
| `segment_rw_32` |884.63 K |888.12 K |+0.39% |

Wasm increased2,597 bytes. The isolated buffer change yields weak scalar gains, worsens both owner
cases and removes none of the five residuals against original0386. The representation and helper
were rejected and removed. These aggregate deltas do not identify an intrinsic allocation cost or
establish remaining necessity. Raw receipts are `segment-prior-candidate-{split,bin,owner}.log` and
`residual-cost/inline-prior-complete.json`; snapshots remain external under `prior-values-inline/`.

One native contract is retained: `prepared_exact_prior_checks_late_edits`. Four cheap two-region
`FailpointMemory` fixtures reuse the existing admission snapshot. Source IDs are distinct, and the
changed removal occurs at ordered position3 or4. Successful split/retirement verifies independent
complete spans, stats, validation and reopen. A source-index value change after preparation must
return exact `BinInvariant` and preserve the reached-source bytes/cache. No representation field is
used to derive the expected result. PocketIC constructors/installs remain0.

The temporary three-entry comparison mutant correctly rejected position3, then wrongly admitted
position4; the new test failed at that exact admission assertion. The complete comparison passed
before the buffer change, with the control, and again after restoration. The test was renamed from
`prepared_exact_prior_covers_inline_and_overflow` without changing its body. This protects ordered
admission, not an unretained inline representation. `segment-prior-mutant-red.log` and
`prior-values-inline/closure.json` bind the counterexample and all stages.

Current production bytes are exactly v4; only this92-line native test remains. Fresh17/34/708 and
scoped formatting pass. Clippy still reports the same41 baseline messages and locations; workspace
format still fails only on preserved unrelated paths. Fresh final seven costs exactly reproduce v4,
and the final wasm matches v4's hash and6,941,727-byte size. It is a current-source artifact, unlike
the discarded target at v5. Final receipts are `segment-prior-final-*`,
`residual-cost/inline-prior-reverted-final.json` and `prior-values-inline/final/`.

No production optimization is retained. Performance remains HOLD+3.8–6.4%; neither removal,
impossibility nor a necessity exception is established. The bounded read-only review ended at100s
with exit124 while waiting on its delegated review, before examining the scoped code/evidence or
returning a verdict. It is INCOMPLETE, not approval; no retry or additional Rust work was performed.
`segment-prior-test-review.log` records this missing independent review.

## Planned single-work-image redesign

**Status: Experimental trial rejected; no production change retained.** Plan0387 authorized one
candidate and two explicit error-contract changes: caller-order staging could return `IndexFull`
before a later `OverlapNext`, and late insertion-source invalidation could return `BinInvariant`
instead of `DuplicateStart`. Both changes reached exact native REDs on old production and GREENs
on the candidate. Approval covered the trial, not adoption.

The candidate used map-owned `DraftEdits`, `LocatedEdit` and `DraftScan` with one prospective image.
It shared the private packed-byte/read owner and editor, removed the allocator key overlay and late
intent replay, and validated observed canonical source windows before readiness. It introduced no
module, dependency or persisted layout. The frozen candidate is external under
`segment-implementation/single-work-image/candidate/`; `candidate.patch` preserves its exact delta.

### Trial result

All costs below compare the fresh unchanged-source baseline with the candidate logs, without
`--persist`. Embedded canbench percentages compare older saved YAML and are not this comparison.
Totals and calculated deltas are approximate at terminal display precision; `comparison.json`
also records rounding intervals. Heap/stable increases are0 pages in every case.

| Case | Fresh pre-trial baseline | Candidate | Candidate vs fresh baseline |
| --- | ---: | ---: | ---: |
| Map scalar total97 |580.23 K |788.13 K |+35.83% |
| Map missing64 |84.01 K |138.66 K |+65.05% |
| Map replace64 |96.07 K |135.82 K |+41.38% |
| Map split/directory |29.12 K |30.56 K |+4.95% |
| Map tail pairs64 |366.09 K |478.16 K |+30.61% |
| `tkbfs_256` |1.03 M |3.60 M |+249.51% |
| `tkbfs_1024` |1.01 M |3.87 M |+283.17% |
| `tkbfs_4096` |1.06 M |4.40 M |+315.09% |
| `lbin_256` |837.26 K |2.98 M |+255.92% |
| `lbin_4096` |1.63 M |4.53 M |+177.91% |
| `segment_rw_2` |86.52 K |111.58 K |+28.96% |
| `segment_rw_32` |884.63 K |1.17 M |+32.26% |

Raw transcripts are `segment-single-image-baseline-{map,tkbfs,lbin,owner}-cost.log` and
`segment-single-image-candidate-{map-costs,tkbfs-cost,lbin-cost,owner-cost}.log`.
The comparison receipt is `segment-implementation/single-work-image/comparison.json`. These
aggregate results reject the candidate; they do not isolate source-proof, scratch or handoff cost,
establish residual necessity, or prove that another design cannot remove the earlier regression.

Candidate native runtime passed36 map/710 LARA in63.33 command-seconds; ten ownership doctests and
scoped formatting passed. Omitting source validation reached `value: changed source admitted`;
reselecting during located staging reached `(2,0)` versus `(1,0)`. Both sensitivities failed at their
advertised assertions, and exact correct candidate hashes were verified before withdrawal.
Scoped canbench-feature Clippy failed with the same40 primary messages/locations on baseline and
candidate, none in the two changed files. This is a different scope from the earlier41-diagnostic
non-canbench gate, which remains FAIL.

The two Rust files were restored to their frozen pre-trial dirty images, including all original
assertions and the retained92-line exact-prior witness. All eight added trial tests were withdrawn.
Fresh restoration runtime passed30 map/708 LARA, including allocator34 and bucket owner17; the five
original doctests and two-file format passed. Fresh scoped Clippy and workspace formatting remain
FAIL. Receipts are `segment-single-image-restored-{native,doctests,format,clippy,workspace-format}.log`
and `segment-implementation/single-work-image/restoration.json`. Production is still v4 plus the
retained test; the proposed new API/errors and changed error precedence are not adopted. ADR0088
and `design/storage/lara.md` retain their exact pre-trial contract bytes.

Contemporaneous producer-bound Wasm receipts were not captured before the trial commands; later
artifact hashes do not fill that gap. Static Wasm handoff/copy and separate source-scratch accounting
remain incomplete. Target Wasm files still contain discarded candidate builds, not current-source
artifacts; no post-restoration benchmark rebuild is claimed. Independent candidate review was not
requested after decisive rejection. The earlier retained-test review stays INCOMPLETE, required
lint/format and0386 final-plan gates stay FAIL, and performance stays HOLD. Plan0387's adoption
criteria remain open; no further optimization or width declaration is authorized.

The first restoration-metadata check stopped before writing source because authorization had
inventoried its own still-open empty log. The owning capture code explains that mismatch. The
corrected check preserves all1019 other captured evidence files exactly and binds the completed
producer log's JSON to the authorization. Neither the log nor historical receipts were rewritten.
The failed command and its correction remain in the continuing ledger.

The remaining subsections preserve the rejected proposal's ownership rationale. Their API sketches
and proposed behavior describe the withdrawn candidate; current contracts remain those in ADR0088
and the storage document.

### Work being removed

`FreeSpanEdit` currently represents prospective by-start state as an ordered `index_edits` vector.
`edited_id` searches it backward; neighbor queries repeatedly search the canonical map while skipping
removed keys, then scan inserted keys. Later, `FreeSpanPreparation::ready` gives the same intent
sequence to `prepare_edits`, which selects directories and pages again and constructs the real map
image. The expected-prior vector compares those two independently computed views.

The map already has the needed state owner: `EditImage`, its canonical-plus-pending reader, and
`EditPlan::lookup` / `apply_edit`. The limitation is its batch-only preparation interface. LARA
cannot query that image while deciding the next coalescing edit. Adding another allocator index or
copying page geometry into LARA would duplicate that owner's knowledge.

| Inspected path | Proposed removal | Surviving owner / work that remains |
| --- | --- | --- |
| `FreeSpanEdit::remove` → `indexed_id` → `edited_id` / canonical `get`, then map `apply_edit` lookup | Reverse intent scan and the second exact-key selection for the same removal | Map selects the removal once; LARA validates the returned ID against its prospective record; map consumes that same selection |
| `previous_or_equal_span` / `next_span` | Canonical searches over deleted entries and scans of every inserted intent | Map queries its prospective directory and entries; LARA still validates record identity, length and overlap |
| `FreeSpanPreparation::ready` → `prepare_edits` | Replay of every index intent and late construction of another prospective view | Ready validates observed source bytes and admits the already-built map image; it does not rerun directory/key searches |
| `index_edits` plus allocator `previous` | Allocator-owned key overlay and parallel expected-prior buffer | Map owns actual ordered priors; LARA checks each prior before accepting that intent |
| `rebuild_heap` source-index iteration plus candidate/source-ID exception | Reconstruction of which source IDs remain indexed after edits | Prospective map iteration supplies the final active IDs; LARA owns record checks, heap capacity and maximum selection |

This removes duplicate *selections*, not every repeated read. Removing one key can change page
positions, minima or directory membership, so the next intent must select against the updated image.
A predecessor/successor query does not authorize reuse of both neighbor locations after the first
removal. Best-fit bin scans, record/bin decoding, dirty-range normalization and heap traversal remain.

The initial `prepare_replacement` loop validating every retired span against the original source
also remains. It runs before prefix consumption; otherwise removal of the selected free span could
hide an invalid retirement overlap. It is a distinct source-ownership check, not removable replay.

### Ownership and interface shape

The existing generic map module owns the new drafting phase. Its interface must remain about
`u64` keys/values and ordered edits; no FreeSpan, bin, coalescing or bucket terminology enters it.
No new module, dependency, persistent field or layout is proposed.

The proposed map additions are an opaque draft, an opaque located intent and an owned draft scan.
The scan was proposed for prospective heap reconstruction: a source-read failure consumes its
loan, without a reusable failed iterator or a caller callback. The trial compiled this interface
shape and then withdrew it:

```text
bound map loan → draft with one EditImage
  draft read queries → prospective predecessor-or-equal / successor
  draft → owned scan → consuming next / finish → successor draft
  draft + one Edit → located intent with its actual prior
  located intent → stage that exact edit → successor draft
  draft → seal source/backing admission → PreparedEdits
  PreparedEdits → publish() or discard()
```

A located intent owns the draft while LARA checks its record/bin state. It exposes the prior value,
not directory positions, page IDs, offsets or byte windows. Staging consumes it and uses its private
selection without another lookup. No other map edit can occur between selection and staging.
An error drops the owned draft; a caller cannot ignore an edit failure and seal partially updated
scratch. This avoids a reusable poisoned draft, rollback log or a callback-based transaction API.
Read-only query failures propagate through the private allocator preparation and discard that
operation; they do not count as completed edit intents.

Scalar and batch preparation retain their current signatures and use the same private selection
and mutation dispatch. Split `apply_edit` at its existing lookup/dispatch seam only as needed:
ordinary calls select and dispatch immediately; a located intent selects, lets LARA inspect the
prior, then invokes that same dispatch. There is still one split/recycle/drain algorithm,
`admit_image` and `publish_image`. Read-only prospective navigation must share the selected-page
codec/validation and search primitives, including checks on a touched neighboring page. It must
not grow a second page validator or turn corruption into absence.

The loan needs two concrete carriers already required by callers: a direct exclusive map borrow
for the existing public batch, and an owned `RefMut` for deferred allocator preparation. A private,
closed carrier in the map can retain either. It cannot accept an arbitrary `DerefMut` destination,
a raw pointer, an owner-ID registry or a cloned Memory handle as publication authority.
The prepared publisher has no destination argument and consumes its loan once.

Owning the `RefMut` makes the returned draft representable without borrowing a field of itself.
`FreeSpanEdit` keeps its record/bin/header state separately and passes the map draft through its
consuming index steps. `FreeSpanPreparation` can hold one optional pending pair of allocator image
and map draft, replacing its separate guard plus replay buffers. `ready` takes that pair and seals
the draft; `PreparedFreeSpans` owns the resulting map capability. The guard stays live through index
publication or discard. Cache installation continues to use prepared heap data without map reads.
`SegmentRewritePreparation` still validates its source before allocator ready admission and keeps
slab → vertex → allocator publication order. The trial supplied native and compile-fail evidence
for both loans and the integrated ready lifetime; no candidate capability is retained.

### Ordered priors

For every accepted edit, the actual prior is read from the image immediately before that edit.
LARA checks `Some(exact_record_id)` for removal and `None` for insertion. Domain expectations stay
with LARA; the map only reports the actual prior and applies the generic edit. A record mismatch,
duplicate start or overlap discards all unpublished state. Successful edits append their prior to
the map's existing result storage; its length is the sole completed-intent ordinal. Queries do not
increment it. Repeated keys, missing-remove `None`, drain/reset and page reuse retain caller order.
A later removal cannot rescue an earlier `DirectoryFull`.

There is no three-entry comparison limit and no allocator copy of the map's prior-value sequence.
The current `prepared_exact_prior_checks_late_edits` obligations at positions3/4 remain, including
exact successful spans/stats/reopen and exact `BinInvariant` on the injected removal-source change.
Changing private test access to the new loan cannot remove that reached rejection assertion.

### Read lifetime and stale detection

Building the image early moves actual-prior discovery earlier. Comparing priors only while drafting
would miss the source-index change that the retained test injects between preparation and ready.
Holding a guard does not, by itself, replace that witness: the current test mutates through the
preparation's held map. A header-only stamp also misses its value-only change, which leaves lengths
and page geometry unchanged.

The deferred draft therefore needs map-owned observations of the canonical bytes used to construct
its result. Their lifetime is one draft through seal, not the store lifetime. Observations cover
source metadata and selected directory/header/key/value probes, plus canonical source portions of
bulk moves. Pending bytes are read from the sole prospective image. Repeated observations must use
the first observed canonical bytes; contradictory later backing reads reject instead of silently
mixing source versions. Seal compares the observed source ranges against current backing before
returning ready. It checks the original bytes, not the staged output, and does no key search/replay.
A removed entry's original value must remain checked even if its destination is no longer live.

This is additional immutable source evidence, not a second prospective map. It has a cost and
requires a private representation. Reuse the existing interval machinery where its invariants fit;
do not put read-only evidence in the dirty publisher or add a general cache/transaction framework.
The proposal below defines the representation and stale-error surface implemented only in the
withdrawn trial. Checking a touched page includes the data used to establish absence and neighbor
selection, not merely a value address. The retained late-removal `BinInvariant` stays exact.
Late-insertion rejection was an explicitly approved trial contract change and is not adopted.
Independently reopened/raw-memory aliases remain outside exclusive region ownership; controlled
source mutation in a fault witness does not establish support for arbitrary concurrent aliases.

The read unit is a selected query/intent's checked byte windows: small directory/key/value probes
and the existing32-byte header/first-key prefix. No unconditional1048-byte page read is proposed.
Only an actual interior shift/split justifies a larger source interval. A located intent expires
when staged; no saved directory/page position survives another intent. Ready retains encoded
writes only after source and full allocated-page extent admission.

The existing synchronous scalar/batch calls have a closed preparation interval and need no new
source-observation cache. They must keep their existing basic-path traffic/scratch contracts. Both
lifetimes use one editor; source observation belongs to the deferred draft, behind the existing
private read-probe seam. The representation must not introduce a public mode flag or an optional
second editor. Any mandatory full-page buffer or source journal/vector in the immediate scalar path
fails this design. Deferred observation overflow is fallible and its allocation/copy cost is charged
in the allocator/segment comparisons; no unconditional reserve is assumed to make that cost free.

### Proposed source capture

The current read seam is insufficient for fallible source capture. `EditImage::read` and
`EditPlan::read/u64` are infallible; lookup probes are `Fn` without an error result, and
`EditImage::cover` accepts an infallible gap filler. Bulk directory/page movement also calls
`Memory::read` directly. Logging only `lookup` would omit mutation sources; hiding allocation
failure inside a Memory wrapper would let the editor continue with incomplete evidence.

The proposal changes that existing private read seam to a fallible probe and propagates failures
through the one editor. New deferred queries report `PrepareError::Read { source: Error }` for
invalid source geometry, including unrepresentable source-page addresses; no requested intent or
fake edit index is claimed. Scratch exhaustion keeps its precise cause. It covers initialization
of source bounds/counts, metadata/key/value reads and canonical gap fills in
insert/remove/split/directory movement. Direct scalar/batch probes return
success after their existing Memory read and instantiate no observed-source storage. Deferred
probes retain evidence. This is a private read contract, not an external transaction callback,
public mode flag or a second editing algorithm.

The packed-range invariant is already owned by `EditImage::cover`: sorted disjoint descriptors and
packed bytes with no dead images. Extract that representation inside the same file into one private
byte-window owner, used by written bytes and observed original bytes. Keep the existing inline
three descriptors/32 bytes and fallible overflow; do not copy normalization or introduce a second
range schema. `EditImage` keeps prior results and backing/required extents. The deferred draft alone
holds an observed-source instance. Its metadata contains no staged values or persistent epoch.

For each canonical read, reserve observation scratch before exposing the result, read the exact
requested backed bytes, compare any overlap with earlier observations, then copy only missing
original bytes into that range owner. Gap filling uses those already-read bytes; it must not reread
the backing to fill an observation. Overlapping evidence is never replaced by newer source bytes.
Fully staged reads and synthetic new-page padding add no canonical observation. Source capture is
fallible before ready; an error drops the draft/located intent/owned scan and all unpublished state.

Seal streams comparison of the recorded ranges through a fixed metadata-sized buffer, proposed32
bytes, before map backing admission. It allocates no comparison vector, performs no directory/key
search and writes no canonical bytes. Physical capacity may have increased; capacity is not an
exact-byte stamp. Check continued backing of observed ranges separately from byte equality and
retain full allocated-page extent admission for newly minted pages. Once ready, only the existing
encoded publisher can write. No source buffers or validation are reachable from that publisher.

Proposed generic error: `PrepareError::SourceChanged`, with no invented edit ordinal. It means the
observed source for the whole deferred draft changed, whether during capture or before seal.
It is not a requested-edit failure. Existing scalar/batch failure variants and their indices remain
unchanged; the new `Read` case is confined to deferred read operations. Observation scratch
failure retains `Scratch { at: Some(completed_intents), ... }` inside an edit, and `at: None` for
read queries. Add exhaustive Display/source and allocator projection
handling; this enum has no stable/Candid representation. The repository consumer audit found the
allocator projection in `FreeSpanStore::map_index_prepare_error`, plus owner-crate matches/tests.

The allocator projects `SourceChanged` to `BinInvariant` for both late removal and late insertion.
A duplicate observed during ordinary intent selection still returns `DuplicateStart`. Today a late
canonical insertion at an expected-absent key can instead reach the final prior comparison and
return `DuplicateStart`; changing that to source invalidation at seal is explicitly proposed, not
an assertion of unchanged behavior. A native witness must record old/new exact errors and preserve
the reached-source bytes/cache. Raw alias mutation is confined to fault injection, not a supported
concurrent ownership mode. Source capture and owned scan costs must be measured before retention.

### Proposed admission order

The source shows a real ordering difference: today LARA completes prospective record/bin/range
validation, heap preparation and record-backing reservation before ready runs map semantics and
growth. Incremental map staging can encounter an index error before a later allocator-domain
error. A concrete proposed witness has a full directory, with its first page holding64 source spans
`(start=4*i, len=1)` for `i=0..63`, and later pages starting above252. Prepare a zero-length destination
with ordered retirements `(2,1)`, then `(1,2)`. Both pass the initial original-source checks. The first
retirement fits between source spans but would split the full page. The second overlaps the first
retirement, so today's private allocator simulation returns `OverlapNext` before map preparation.
Incremental staging returns `IndexFull` on the first insertion. The trial's real full-directory
fixture executed this counterexample: exact old `OverlapNext` RED, then exact candidate `IndexFull`
GREEN, with the reverse-order overlap and preservation checks passing.

Map byte/range scratch also moves earlier, while full allocated-page backing grow can stay at seal.
A reservation count is not directory admission. Known batch result reservation and its exact error
index must remain intact; a deferred allocator bound must not silently cap actual results or
reclassify a scratch failure as corruption. Source revalidation must precede readiness, and no
fallible admission may follow the first canonical write.

The proposed contract keeps existing zero/no-op and caller input guards, including exact-pair
input-validation precedence and all retirement checks against the original source before allocation.
It then processes prospective mutations in caller order: domain record/bin checks for the current
intent, that intent's map selection/staging admission, then the next intent. Stop at the first
returned error. All physical record/index backing, heap capacity and source revalidation still
complete before canonical publication. There is no promise that later domain errors outrank an
earlier map staging failure. The concrete full-directory case changes `OverlapNext` to `IndexFull`;
reversing the retirements must still reach the exact overlap error when the first staging fits.

This order was approved for the trial and is not adopted. Its requirements preserved every tested
input error and exact compound-error characterization without accepting `old | new`. Do not add a
second domain dry-run, replay fallback, permissive out-of-capacity map image or an ignored-key
neighbor overlay. `replace_exact_pair_with` must keep its input guards and perform subsequent
neighbor selection against its one prospective map after tentative removals. If these requirements
cannot be met without another state owner, stop and reject the candidate.

### Alternatives and decision gates

Candidate1, block-reading bin records, changes the physical read unit with a smaller interface
change but leaves the duplicated by-start simulation/replay. It remains a comparison hypothesis,
not an additional authorized trial. Candidate3, replacing by-start/bins/heap with an interval index,
changes persistent layout and selection behavior; it would require a separate ADR and fresh owning
store bundles. No compatibility decoder, migration or rebuild path is proposed. Rejected inlining,
scratch-reservation, inline-prior and rekey controls are not revived.

The required-axis review supports candidate2's ownership direction with changes: page facts stay
encapsulated at the map; range/bin policy stays at FreeSpanStore; one prospective map removes the
index-overlay consistency surface; consuming loans enforce publication authority. Fitness remains
conditional on source-evidence cost and admission/error order. Three opaque drafting/scan types,
the closed loan carrier, one shared private byte-window representation and immutable source
observations are the proposed added concepts. `Read` and `SourceChanged` add nonpersisted error
cases for query corruption and whole-draft invalidation without fabricated intent indices. They must
pay for deletion of the overlay/replay, not become another editing layer. Owned draft/intent
handoffs also require inspection against the earlier caller-owned-image work; avoid restoring its
image-copy/stack cost. No speedup or residual-necessity claim follows from source inspection.

The trial required these gates before retention; failed performance and incomplete evidence prevent adoption:

- Native map queries after insert/remove, changed minima, cross-page neighbors, recycled pages and
  drain/reset; exact keys/values and full reopen after publish, unchanged bytes after discard.
- Ordered-prior and selected-page corruption tests, missing/remove boundaries, scratch/grow failure
  and retry, including the four existing compile-fail ownership cases and both loan carriers.
- Retained positions3/4 source-change witness, plus absence/neighbor source changes and mixed-error
  characterization. Sensitivity must reject omitted source revalidation and repeated intent selection.
- Allocator exact-pair, original-source retirement check, best-fit/prefix/restore and heap-rebuild
  contracts, including cold-cache record-ID recycling and byte/cache preservation on every error.
- Existing map basic scalar work oracle and direct map scalar controls, all five unchanged allocator
  cases and both unchanged segment-owner cases. Charge source reads, scratch and handoff costs;
  preserve results/prechecks and compare with both original0386 and v4. No benchmark persistence.

The trial added and executed the owning-layer tests described by
[plan0387](../../plans/0387-single-work-image-preflight.md), then withdrew them with the rejected
implementation. PocketIC constructors and canister/federation installs stayed0. ADR0088 and
`design/storage/lara.md` need no new API contract because the exact pre-trial implementation is
restored. This outcome closes no0386 completion criterion and authorizes no additional candidate.

`segment-implementation/single-work-image-design/` preserves the documentation-only source and
prior-evidence binding after checkpoint-v6. That checkpoint remains unchanged and historical to
its exact document bytes; it is not rewritten to imply approval of this design. The fresh
`segment-single-image-design-plan-check.log` still reports FAIL for the same pending TODOs and open
criteria5/6. That earlier documentation-only phase added no Rust, benchmark or independent-review
result; the separate trial and restoration receipts above record the later execution.

## Residual cost attribution (measured; binding partial)

Status: eight measurements complete; exact Rust restoration complete; binding partial; causal
attribution unresolved; performance HOLD. [Plan0388](../../plans/0388-residual-allocator-attribution.md)
used the approved1260-second allocation within the unchanged4910 ledger ceiling. Its frozen draft,
source allowlist and probe placements remain in
`segment-implementation/residual-attribution/execution/authorization.json` and the approval snapshots.
No production optimization, test or fixture change is retained.

All four successful Wasm producers have exact source/config/runtime receipts and external archives
under `segment-implementation/residual-attribution/epochs/`. Current canonical exactly matches the
historical v4 Wasm SHA256 and6,941,727-byte size. The two existing256 fixtures each retain16 take/restore
pairs, their setup assertions, maintenance and inclusive parent. Exact CSV instruction totals are:

| Epoch | `tkbfs_256` | `lbin_256` |
| --- | ---: | ---: |
| Original canonical | 974,459 | 786,986 |
| Current canonical | 1,027,928 | 837,265 |
| Original scoped | 1,473,642 | 1,194,918 |
| Current scoped | 1,513,926 | 1,240,035 |

Canonical residuals are+53,469 (+5.4870%) and+50,279 (+6.3888%). The earlier split+5.7% was calculated
at1.03 M terminal precision; the exact paired CSV comparison reproduces the same v4 regression.
No five-case or segment-owner acceptance run was performed.

The named `fs388_` guards use the existing canbench feature/Wasm target and close before publication.
Their eight intervals are disjoint in the inspected call graph; neither publisher contains a new
guard. Counts match original/current: constructor/heap/image/index/priors32, coalescing16,
remove/insert31 for split and16 for largest-bin. The original insertion-only final check and current
complete ordered-prior comparison retain their respective existing behavior.

| Diagnostic interval | Split current minus original | Bin current minus original |
| --- | ---: | ---: |
| Constructor | +24,387 | +20,448 |
| Coalescing | +4,424 | +2,896 |
| Remove | +4,749 | +2,517 |
| Insert | +629 | +208 |
| Heap preparation | +880 | +848 |
| Final image | −25,929 | −23,247 |
| Map preparation | −10,012 | +836 |
| Final priors | +16,497 | +16,272 |
| Disjoint child union | +15,625 | +20,778 |
| Parent minus child union | +24,662 | +24,339 |

The remainder contains selection, handoffs, publication and measurement bookkeeping. Parent and
children are never added together. Constructor/prior growth directs attention to admission and
ordered comparison; final-image savings exceed the measured coalesce/remove/insert increases.
Heap preparation alone grows less than900 instructions per case. These observations neither isolate
an allocation/codec/copy cause nor establish a necessary safety-cost exception.

Probe effects add499,183/407,932 instructions to original canonical totals and485,998/402,770 to current.
Whole totals inflate47.2794–51.8347%. The differing probe effects explain why scoped whole deltas are
13,185/5,162 instructions smaller than canonical deltas; that difference has no causal production
phase assignment. All recorded heap/stable increases are0 pages, which does not imply0 allocations.
The unchanged map owner also has different scope deltas across cases, so caller/generated-code and
measurement effects remain relevant.

The next falsifiable question is whether paired trace/generated-code evidence on the archived
canonical binaries puts a common excess in `FreeSpanPreparation`/`PreparedFreeSpans` handoffs.
Reject that explanation if the excess lies elsewhere or disappears without probes. This requires
another reviewed slice; no fresh trace, optimization control or gate closure follows from0388.
Full tables and interpretation are in
`segment-implementation/residual-attribution/execution/attribution.md` and `attribution.json`.
The earlier read-only trace/source findings remain frozen under the parent directory.

The binding protocol is incomplete. Original canonical, original scoped and current canonical have
hashes before/after their two-case epoch intervals, but lack separate contemporaneous hashes at each
of their six case boundaries. Current scoped has both boundary receipts per case. Later hashes cannot
supply the missing observations, and no case was repeated to hide that gap. Plan0388 keeps the binding
criterion and final-approval criterion open. The final structural command reports FAIL for exactly
criteria2/7: `segment-residual-attribution-plan-final.log`, exit1. It was not retried.

All nine Rust files are exactly restored to their frozen current snapshots; no guard remains. Both
restored native commands pass708/708, including allocator34, owner17 and
`prepared_exact_prior_checks_late_edits`. Scoped and restored allocator formatting pass. Fresh scoped
Clippy remains FAIL with the same40 primary occurrences/39 distinct message-location pairs as the
restored0387 baseline. The earlier41-message and workspace-format failures remain unchanged;
independent review remains INCOMPLETE.

Receipts are `segment-residual-attribution-original-half-native.log`,
`segment-residual-attribution-final-{native,clippy,format}.log` and
`segment-implementation/residual-attribution/execution/{clippy-comparison,pre-document-closure}.json`.
The original undo metadata timeout remains recorded: source writes completed and later diagnostics
and native validation proved exact restoration, without rerunning undo. Final source-only undo
passed in0.08 seconds. The evidence helper separates undo from full-history verification; a count
parser's occurrence-versus-distinct assumption was also corrected only in metadata.

The target Wasm is current-scoped, while the archived current-canonical Wasm is the uninstrumented
control. No post-restoration release rebuild, benchmark persistence, PocketIC E2E fixture, stage,
commit or push is claimed. ADR0088 and `design/storage/lara.md` retain their exact contract bytes.
`segment-implementation/residual-attribution/execution/final-checkpoint.json` binds the final document
and terminal state. The missing case-boundary receipts prevent full0388 closure; performance remains HOLD.

## Canonical caller attribution (four traces complete)

Status: diagnostic execution and accounting complete; causal mechanism unresolved; performance
HOLD. [Plan0389](../../plans/0389-canonical-allocator-traces.md) captured four independently bound
instruction traces on the archived original/current canonical Wasm. The positive generated-code
cost is concentrated in the FreeSpan execution family in both256 fixtures. No source switching,
build, new scope, optimization or persistence occurred.

Evidence is under `segment-implementation/canonical-handoff-attribution/` (`C`). Its frozen
`analysis.md` records the preceding static phase; `execution/analysis.md` records this execution.
`execution/trace-attribution.json` is the current exclusive accounting source, with
`execution/final-result-projection.json` as its smaller projection. `execution/authorization.json`
binds the four-case schedule and585 continuing seconds, within the unchanged4910-second ceiling.
Each `C/traces/<epoch>/<case>/` contains its own CSV, SVG, capture and contemporaneous before/after
input receipts. Archived Wasm/runtime/CLI/config/source/producer hashes match both boundaries.

| Direct trace command receipt | Whole CSV instructions | Parent CSV instructions | SVG exclusive sum | SVG minus CSV | Exit / seconds |
| --- | ---: | ---: | ---: | ---: | --- |
| `segment-canonical-trace-original-split.log` | 974,459 | 973,508 | 974,659 | 200 | 0 / 87.99 |
| `segment-canonical-trace-original-bin.log` | 786,986 | 785,938 | 787,200 | 214 | 0 / 67.93 |
| `segment-canonical-trace-current-split.log` | 1,027,928 | 1,026,887 | 1,028,128 | 200 | 0 / 48.20 |
| `segment-canonical-trace-current-bin.log` | 837,265 | 836,222 | 837,465 | 200 | 0 / 29.84 |

All four traced CSVs reproduce their saved uninstrumented canonical controls. Parent scope calls
remain1, heap/stable growth0, and each unchanged fixture has16 take/restore pairs. The canonical
whole deltas remain+53,469/+50,279 (+5.4870%/+6.3888%); traced CSV whole effects are0. Runtime durations
are receipt metadata. SVG boundaries differ from CSV: split's SVG delta matches53,469, but bin's
50,265 differs by14 because the offset changes214→200. That difference remains explicitly unassigned
outside the generated-function partition; its cause is not established.

One SVG reader validates integer Inferno intervals, unique immediate-parent containment,
nonnegative self counts and root/self sums equal to each SVG total. Same-depth intervals cannot
overlap. Every exclusive instruction is charged once; inclusive parent/child views are never added.
The single function-name policy includes FreeSpan namespace-containing generic/drop/benchmark
bodies, paged-map and stable-memory names, standalone copy names and allocation machinery. Rust
allocation/deallocation/reallocation wrappers are grouped with raw-vector and dlmalloc callees.
Names identify generated locations; inlined source intervals remain inseparable.

| Exclusive generated name family | Split original | Split current | Split delta | Bin original | Bin current | Bin delta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| FreeSpan-named | 180,871 | 258,424 | +77,553 | 212,298 | 273,792 | +61,494 |
| Paged-map-named | 628,896 | 627,013 | −1,883 | 369,193 | 375,312 | +6,119 |
| Stable-memory-named | 56,076 | 40,022 | −16,054 | 116,067 | 112,402 | −3,665 |
| Allocation machinery | 92,660 | 86,527 | −6,133 | 78,881 | 65,185 | −13,696 |
| Standalone copy-named | 15,297 | 15,286 | −11 | 9,904 | 9,912 | +8 |
| Other | 859 | 856 | −3 | 857 | 862 | +5 |
| SVG total | 974,659 | 1,028,128 | +53,469 | 787,200 | 837,465 | +50,265 |

Source and sealed same-memory fragments establish changed placement: original release delegates
to replacement, while current release constructs its edit and directly reaches shared preparation/
ready/publication. Constructor/remove/insert work was inlined originally and is outlined currently.
The selected self unions below match that shift; they overlap and are not extra partition rows.
Missing original symbols mean work in the caller, not zero source cost.

| Source-matched exclusive view | Split original | Split current | Split delta | Bin original | Bin current | Bin delta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `replace_spans` | 116,634 | 55,202 | −61,432 | 81,068 | 48,185 | −32,883 |
| `release_coalesced` | 12,036 | 55,020 | +42,984 | 5,327 | 51,139 | +45,812 |
| Both scalar callers | 128,670 | 110,222 | −18,448 | 86,395 | 99,324 | +12,929 |
| Callers plus outlined new/remove/insert | 128,670 | 169,611 | +40,941 | 86,395 | 140,708 | +54,313 |
| Callers plus those and edit release/coalescing | 128,670 | 181,962 | +53,292 | 86,395 | 148,439 | +62,044 |

The simple prediction of a common positive sum in the two same-named caller frames fails for
split. Matching outlined edit operations instead gives a positive cost in both paths. These views
include admission/record manipulation and inlined prepare/ready/publication; they do not isolate
carrier transfers or complete prior checks. Source now stores encoded prospective records and
retains complete priors with changed scratch admission. Record self increases while memory-helper
self decreases. Allocation machinery self decreases in both cases, so increased exclusive allocator
backend work does not explain the regression; in-caller scratch admission remains unresolved.

A selected selector self union (`pick_span_in_bin`/`find_best_fit` plus the split take caller) changes
+6,935/+2,545; those names alone do not define an identical semantic phase. Map-family changes are
−1,883/+6,119, including publisher self changes+108/+2,735. Standalone copy-name deltas do not measure
inlined Wasm `memory.copy`. The per-function/direct-child data and source matching are retained in
`execution/analysis.md` and the accounting JSON.

LLVM23.1.1 static facts remain: `replace_spans` entry frame528→1024 bytes, bulk-copy sites2→6 and
unweighted static instructions3499→1994; `release_coalesced`80→1024,0→6 and759→1952. Both current
callers contain two104-byte and four152-byte literal stack-copy sites. No dynamic site frequency,
copy-cost multiplication or peak-stack claim follows. Original full export passed with4135 headers;
current timed out after2583/4205. Only153 sealed allocator-namespace bodies before the unfinished
tail were consumed; no complete current export or retry is claimed.

The first trace analysis reconciled all four SVGs, then failed an unsupported equal-offset
assumption for bin. The owning comparison now preserves the actual14-instruction difference;
corrected metadata analysis passed. Allocator wrappers were then grouped by the one shared policy.
Failed/preliminary metadata outputs remain audit records, not current attribution. No runtime case
was repeated and no capture output was rewritten. Earlier static snapshot/export timeout receipts
remain exact.

Closure binds exact source/fixture/result/prior-evidence bytes, HEAD/index/status/path manifests,
document/reference checks and the final structural result in `execution/final-checkpoint.json`.
`segment-canonical-trace-plan-final.log` and `segment-canonical-trace-diff-check.log` record the
bounded terminal checks. No Rust/cargo/rustfmt checks were added for this source-read-only slice.
Historical708-test passes remain historical; Clippy40 occurrences/39 pairs FAIL, workspace format
FAIL, independent review INCOMPLETE and performance HOLD remain. Plan0388 criteria2/7 and its exact
final structural FAIL remain unchanged; four new receipts cannot repair its six missing historical
case-boundary observations. There is no production candidate, benchmark persistence or stage/
commit/push. Four existing canbench executions were real PocketIC runtime work, not zero benchmark
canister installations. No API/layout contract changed, and `design/storage/lara.md` stays exact.

The next reviewed question is whether the positive scalar-family cost is predominantly in the
inlined preparation/ready moves and prior-comparison regions or constructor/admission/encoded-record
work. An instruction-region trace or one invariant-preserving control is needed. Neither that
control nor a production correction is authorized here.

## Binary copy-region attribution (0390)

Status: four source-read-only diagnostic cases complete; explicit bulk-copy contribution narrowed;
remaining mechanism unresolved; performance HOLD. [Plan0390](../../plans/0390-binary-copy-region-attribution.md)
uses surgical changes to archived Wasm, with no Rust recompilation or production correction.
Evidence is `segment-implementation/canonical-region-attribution/` (`D`): `analysis.md`,
`selected-sites.json`, epoch producer receipts, four case captures, `region-attribution.json` and
`projection.json`. User-directed progression and the delegated time allocation are bound by
`D/authorization.json`; the old runner/ledger/0389 authorization remain unchanged.

Local source inspection clarifies the0389 measurement boundary. canbench CLI/SDK0.7.0 uses a
separate ordinary canister for CSV and a function-instrumented canister for SVG. SDK
`adjust_traces_for_overhead` distributes overhead across trace events using the ordinary total.
CSV equality establishes canonical reproduction; it does not establish zero tracing-canister
perturbation.0389's self tables remain corrected generated-location accounting, with unresolved
raw inlined-phase cost. Exact SVG root sums alone cannot resolve that distinction. Cached CLI and
ic-wasm0.9.9 source archives are hash-bound externally; no tool/package was installed. The latter
has function/stable-memory profiling, not a direct instruction-range CLI, and was not run against
allocator regions.

Each selected `memory.copy0 0` was replaced by a direct call to one three-i32/no-result helper
containing only the same three operand loads and original copy. Original indices, locals, branches
and other body bytes stay exact; helpers/type/names are appended. All non-owned sections and
unselected original bodies are byte-identical. Original has2 selected sites; current has13 across
replacement/release/constructor. Copies preserve operand order, length, overlap and instruction
bounds semantics; the diagnostic adds function-call structure. No validation, index/cache behavior
or publisher ownership was removed. The first transformer misread LLVM's symbol coordinate; the
correct owner now verifies its body-size prefix. That failed metadata receipt stays visible, and
no runtime case was repeated.

| Direct receipt | Exit / seconds | Canonical whole CSV | Shim whole CSV | Wrapper effect | SVG total | SVG minus shim CSV |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `segment-region-original-split.log` | 0 / 30.67 | 974,459 | 975,035 | +576 | 975,235 | 200 |
| `segment-region-original-bin.log` | 0 / 23.50 | 786,986 | 787,562 | +576 | 787,770 | 208 |
| `segment-region-current-split.log` | 0 / 26.43 | 1,027,928 | 1,029,944 | +2,016 | 1,030,144 | 200 |
| `segment-region-current-bin.log` | 0 / 25.92 | 837,265 | 839,281 | +2,016 | 839,481 | 200 |

All four have immediate matching before/after input receipts. CSV scope calls1, heap/stable growth0,
fixture cardinality256 and16 take/restore pairs stay unchanged. The retained scoped target Wasm is
not consumed. One reused exclusive reader reconciles integer parent intervals/self sums for each
SVG. Copy helpers are leaf frames, charged once. Original sites each execute32 times; all twelve
current caller sites execute16 times and constructor32 times. Visits are64→224 and copied bytes
8,192→30,976 in both fixtures. CSV wrapper effects are576/64 =2016/224 =9 instructions per visit,
empirically identical across all four cases; the paired wrapper addition is1,440.

| Corrected copy-helper view | Split256 | Largest-bin256 |
| --- | ---: | ---: |
| Original helper self | 8,215 | 8,184 |
| Current helper self | 30,988 | 31,076 |
| Paired helper self delta | +22,773 | +22,892 |
| Canonical residual control | +53,469 | +50,279 |

The helper delta is approximately42.6%/45.5% of the residual. This supports a substantial explicit
bulk-copy contribution; these copies do not explain the entire regression. Helper self includes
parameter/function structure and SDK correction. Identical site visits/lengths still yield a119
instruction difference between the two corrected comparisons. No assumed byte-to-cost conversion,
guessed entry-cost subtraction or exact semantic-phase remainder is claimed. Bin's current shim
SVG/CSV offsets208→200 leave8 instructions unassigned;0389's prior14 remains its own observation.
Scalar/vector transfers, encoded-record handling, scratch admission and complete prior comparison
remain unresolved. The next one-factor diagnostic should distinguish those regions and use ordinary
CSV as its decision measurement, retaining trace costs as a corrected secondary view.

`D/final-checkpoint.json` binds exact prior plan/report/trace/receipt preservation, repository
HEAD/index/status/path manifest, source/result bytes and final document/reference checks.
`segment-region-plan-final.log` and `segment-region-diff-check.log` record structural/whitespace
results. No Rust build/test/clippy/rustfmt, source/API/layout change, production candidate,
benchmark persistence, stage, commit or push occurred. Four existing canbench commands were real
PocketIC runtime work. Historical708-test passes, Clippy40/39 FAIL, workspace format FAIL,
independent review INCOMPLETE, performance HOLD and0388 criteria2/7 remain unchanged.

## Constructor header-placement control (0391)

Status: four ordinary cases complete; this constructor transfer is small; other carrier/record/
scratch/prior work unresolved; performance HOLD. [Plan0391](../../plans/0391-constructor-carrier-control.md)
binds one private generated-code region. Evidence under
`segment-implementation/canonical-carrier-control/` (`Q`) includes `analysis.md`, bound selected
functions/producer, `semantic-check.json`, four captures and `paired-results.json`.

Current `FreeSpanEdit::new` reads a48-byte header into a temporary, validates it, then transfers it
into the prospective edit with three v128 load/store blocks. The diagnostic reads into that edit
location directly, rebases its three validation reads and removes only those redundant transfers.
It preserves the original temporary's four padding bytes because the reader writes only44 bytes
and the unchanged152-byte result copy also carries padding. Constructor indices/signature/locals,
224-byte frame, branches, borrow/backing/count checks, reservations/drop paths and final bulk copy
remain unchanged. Every other function body and non-code section is byte-identical. The external
Wasm is a one-body control, not a production candidate; no Rust/IR/compiler round-trip occurred.

Forty-five external Node/Wasm comparisons use exact constructor bodies and bound dependency stubs.
They preserve result/live-edit bytes, ordered calls, outside-frame memory, counter and stack through
success, constructor rejection, borrow/index traps and five reserve failure/cleanup points. Wrong
padding and wrong validation-read controls are caught. This semantic check is not a whole-store,
OOM/rollback or native test pass; the full-module runtime keeps the actual dependencies unchanged.
Canonical records/bins/index/cache and `ready`'s complete priors remain owned by the existing
FreeSpan preparation/publication boundary. No validation or admission check was removed.

| Ordinary receipt | Exit / seconds | Whole instructions | Scoped instructions | Paired effect |
| --- | --- | ---: | ---: | ---: |
| `segment-carrier-baseline-split.log` | 0 / 11.51 | 1,027,928 | 1,026,887 | 0 |
| `segment-carrier-baseline-bin.log` | 0 / 3.83 | 837,265 | 836,222 | 0 |
| `segment-carrier-header-alias-split.log` | 0 / 6.01 | 1,027,672 | 1,026,631 | -256 |
| `segment-carrier-header-alias-bin.log` | 0 / 4.57 | 837,009 | 835,966 | -256 |

Each case ran once with `--csv`, a `wasm_path`-only external YAML, and immediate matching actual
input hashes. No `--instruction-tracing`, build_cmd or persistence was used. Both baselines
reproduce archived current values exactly; scopes/calls/growth and256 spans/16 take-restore pairs
stay unchanged. Decision costs are ordinary CSV, independent of the prior SDK trace correction.

The256-instruction reduction is0.4788%/0.5092% of canonical+53,469/+50,279 and below the declared5%
material-effect bound. It is the same whole/scoped effect in both fixtures, empirically8 per the32
constructor visits previously observed. This rejects this specific header transfer as the large
residual cause; other caller transfers are unresolved. It neither prices bytes/opcodes nor converts
0390's corrected self into an exact semantic remainder.

Read-only source comparison finds a more specific next candidate: original prospective records
keep `SpanRecord`; current overlay hits decode `[u8;48]` and updates encode it again. This is a
verified representation difference, not a cost proof. A separate one-factor record-staging control
must retain stable encoding at the image owner, reserve-before-publication, complete priors and
cache behavior. No production correction follows from0391.

The initial baseline accidentally captured its active log before completion, and authorization
refused that manifest. `Q/baseline-sealed.json` is the canonical completed-receipt manifest; the
partial snapshot/failed authorization remain forensic. `Q/final-checkpoint.json` binds prior0390
closure/report/plan/evidence preservation and current source/result/HEAD/index/status/path checks.
`segment-carrier-plan-final.log`/`segment-carrier-diff-check.log` bind structural/whitespace results.
No Rust checks, source/API/layout change, new E2E topology, saved-result refresh, stage/commit/push
occurred. Four canbench commands were real PocketIC runtime work. Historical708-test passes,
Clippy40/39 and workspace fmt FAIL, independent review INCOMPLETE, performance HOLD and0388
criteria2/7 remain unchanged.

## Prospective record-staging control (0392, blocked)

Status: authorization error and unchanged external baseline build timeout; representation control/
measurement not reached; performance HOLD. The inherited Rust/cargo/rustfmt/clippy prohibition applies
to external snapshots too. The progression instruction did not explicitly lift it. The agent
incorrectly inferred build permission, executed one Cargo build, acknowledged the error and stopped.
`S/scope-correction.json` corrects `authorization.json`; the original receipt remains forensic. [Plan0392](../../plans/0392-prospective-record-staging-control.md) remains partial.
Evidence under `segment-implementation/canonical-record-control/` (`S`) includes bound baseline/
source/toolchain/common-lock inputs, the failed source/producer, `blocked-checkpoint.json` and
`analysis.md`.0391's256-instruction header result and all earlier copy evidence remain exact.

The next proposed factor is prospective record representation: current overlay hits decode
`[u8;48]` and updates encode again; decoded staging would postpone existing `SpanRecord::encode`
to the current encoded-image owner in `prepare_edit`. The final output needs bounded fallible
scratch before publication, so allocation/stride/encoding are part of the representation-choice
cost. It is not a pure codec price. Record bounds, validation, complete index priors, cache and
`ready`/`publish` ownership must remain unchanged. No decoded source diff was applied or tested.

The unchanged physical external snapshot uses current Rust/Cargo/config/profile and common lock.
One producer attempted release/locked/wasm32-unknown-unknown/canbench with normal shared target
reuse. External local package paths caused four workspace packages to compile. Its child Cargo
reached the90-second timeout (124/90.04s); `segment-record-baseline-build.log` records outer cap95,
exit124/95.02s. Warnings came from unchanged code; no successful `Finished`/Wasm exists. The failed
producer has `artifact:null`, not a build pass. No baseline ordinary reproduction, representation
comparison or cost inference is possible.

Cleanup logged the timeout, restored the retained scoped Wasm and wrote the failed producer before
the outer receipt. Hash/preservation work consumed additional wrapper time beyond its nominal
five-second gap. `segment-record-blocked-preservation.log` independently confirms the actual target
against the original backup, exact retained source/results/history/status/index/path and identical
external source hashes. No source control, candidate Wasm, case directory or ordinary canbench run
exists. Native34-test validation, scoped formatting/clippy and candidate builds/measurements are
explicitly deferred. No build/runtime retry, cold-target/wrapper/terminal fallback or delegated wait
was used. Further Rust/build execution requires explicit lifting of the prohibition as well as a
sufficient complete producer/reproduction budget; an external copy alone grants no permission.

`S/final-checkpoint.json` binds the old report prefix/suffix and prior closure/source/target/evidence
preservation. `segment-record-plan-final.log` reports pending measurement TODOs/criteria; the corrected
final receipt additionally leaves the authorization prerequisite open.0392 is not a final-approval
candidate; the corrected diff-check records whitespace independently. Original verification/final
validator receipts preceded the scope correction and remain forensic, not current approval.
No retained source/API/layout or saved-result change, production candidate, persistence/stage/commit/
push occurred. One real timed-out Wasm build occurred and no PocketIC runtime work ran. Historical708
passes, Clippy40/39 and workspace fmt FAIL, independent review INCOMPLETE, performance HOLD and0388
criteria2/7 remain unchanged.

## Prospective record-staging control (0392, authorized continuation)

Status: external source/Wasm control produced; decoded ordinary split timed out with no CSV, bin
not run; representation cost unknown; performance HOLD. [Plan0392](../../plans/0392-prospective-record-staging-control.md)
is partial. `V=segment-implementation/canonical-record-authorized/` binds explicit `いいよ` to lifting
Cargo/rustfmt/clippy prohibition only in external snapshots and adding300 seconds while retaining
source/results unchanged. The old unauthorized Cargo execution/scope correction/partial closure in
`canonical-record-control` remains immutable and is not retroactively authorized.

The continuing ledger ceiling is4910→5210, not a reset; command/turn caps remain300/600 seconds.
`V/run.py` executes the original frozen runner with only its numeric budget literal substituted;
lock/log/input semantics and old runner bytes remain. The corrected producer owns108-second child/
125-second outer cleanup, terminates only its child foreground session if interrupted and restores
the retained scoped Wasm before receipt/preservation. No shared-target clean, cold target, unrelated
process termination, external-terminal fallback or delegated wait was used. Bootstrap's intentional
terminal-state-label comparison was corrected before Rust execution; the failed receipt remains.

Five external FreeSpan hunks keep decoded prospective records, copy overlay hits/update directly,
and reserve/encode a final output vector at the existing `prepare_edit` image boundary. The stable
codec/layout/errors, constructor admission/source bound, bin heads, full mutation/index/heap logic,
complete priors, ready/publish and tests remain exact. All other copied files are hash-identical.
`V/source-control.patch`/`source-control.json` bind the diff. The factor includes staging stride,
final allocation/encoding and compiler effects, not a pure codec price. All fallible work remains
before publication; no public type/API/flag/schema, unsafe cast or alternative writer is introduced.
The retained implementation remains encoded staging; this is not a production representation change.

Both source-bound Wasm producers pass. Fresh baseline is byte-identical to archived canonical SHA
`8bbe5181766f865955cf7c64345c24b05f5c1c7181e6eb6bbbe2462a56d25293` (6,941,727 bytes), outer95.97s.
Decoded SHA `2cec6974340e524bc3dfeec01e0ac2609d5ae53690f0c8532a1bbc9fe3692be2` (6,945,209 bytes,
+3,482), outer110.89s. Inputs/commands/artifacts/actual retained-target restoration are bound by
`V/epochs/*/producer.json`; successful archived artifacts need no rebuild. The unchanged baseline
ordinary split/bin values reproduce exactly with immediate actual input receipts:

| Case | Whole instructions | Scoped instructions | Scope calls | Heap/stable growth |
| --- | ---: | ---: | ---: | --- |
| split256 | 1,027,928 | 1,026,887 | 1 | 0/0 |
| bin256 | 837,265 | 836,222 | 1 | 0/0 |

Existing34 FreeSpan native tests pass in30.58s (runtime0.44s,674 filtered out), with actual names
matching the frozen inventory. They retain conflict/grow/discard/cache/coalescing/split/restore/
reopen/summary/max assertions; no test or E2E fixture changed. Both touched-file rustfmt checks pass.
Scoped `cargo clippy -p ic-stable-lara --locked --lib --tests -- -D warnings` fails101/7.11s with41
errors, each binding an unchanged other file/site; no control-owner error. `V/source-validation.json`
records exact diagnostics/source hashes. This41 selection does not replace older40/39 receipts.
Clippy remains FAIL, full Rust validation is not claimed, and output-scratch OOM injection remains
a production prerequisite.

Decoded split's exact ordinary command `canbench bench_r_fs_tkbfs_256 --csv --runtime-path
/Users/yota/dev/gleaph-spill/crates/ic-stable-lara/.canbench/pocket-ic`, cwd `V/cases/decoded/
bench_r_fs_tkbfs_256`, timed out at cap15/124/15.06s with an empty log and no CSV. Immediate after
receipt binds unchanged actual inputs/exit before deliberately rejecting success (after command
exit1); no successful capture is minted. Decoded bin is NOT RUN after the stop boundary. There are
two completed ordinary cases and one timeout, no repeat, tracing/SVG/persistence or saved-result
update. Silent timeout establishes neither startup/compilation delay nor semantic/sandbox cause.
No paired instruction cost or negligible/significant record-representation verdict is possible.

`V/observations.json`/`analysis.md` bind the partial result; `V/final-checkpoint.json` binds exact
retained source/target/results/status/index/paths and all prior report/plan/evidence bytes around this
appended section. Final plan validation reports one pending measurement TODO/criterion; references
and whitespace pass separately. The successful artifacts and missing runtime evidence are kept
separate. No production candidate, retained source/API/layout/result change, persistence/stage/commit/
push occurred. Historical708, Clippy40/39 and workspace fmt FAIL, independent review INCOMPLETE,
performance HOLD and0388 criteria2/7 remain. Further work needs diagnosis and an explicitly approved
bounded ordinary schedule against these artifacts, not silent retry/rebuild or inferred copy/header
additivity.

## Prospective record-staging control (0392, ordinary result)

Status: diagnostic complete; decoded-plus-final-output representation is slower; do not adopt it as
the allocator remedy; performance HOLD. [Plan0392](../../plans/0392-prospective-record-staging-control.md)
now has complete ordinary evidence. `W=segment-implementation/canonical-record-runtime/` consumes
the already successful frozen decoded Wasm; there is no rebuild or new source change. The two
fresh/reproduced baseline CSV cases in `V` and two completed decoded cases in `W` bind the comparison.

The user rejected the assistant-introduced15-second deadline and lifetime allowance. `W/permission.json`
removes aggregate-time admission and closure reserves and authorizes the previously interrupted
split plus bin. `W/run.py` retains original lock/log/input/ledger behavior without the cumulative
ceiling. Elapsed history is preserved only as evidence, not used as a reason to stop. Existing project
rust-workflow safeguards remain; no new slice allowance. Earlier timeout/scope errors and all old
source/build/check/report receipts remain exact rather than being rewritten as successful.

Both ordinary measurements pass with immediate before/after actual-input receipts, unchanged
fixture/scopes/tools/runtime/source/artifact, one scope call and zero heap/stable growth:

| Case | Baseline whole | Decoded whole | Whole delta | Baseline scoped | Decoded scoped | Scoped delta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| split256 | 1,027,928 | 1,033,424 | +5,496 | 1,026,887 | 1,032,473 | +5,586 |
| bin256 | 837,265 | 848,193 | +10,928 | 836,222 | 847,240 | +11,018 |

Whole costs increase about0.53%/1.31%. Unscoped delta is−90 in each, so whole/scoped values are not
interchangeable. Whole gaps versus archived original are+58,965/+61,207, above current+53,469/+50,279.
`W/paired-results.json` binds exact deltas/percentages/captures. This complete alternative recovers
none of the regression; it includes final output allocation/stride/encoding/compiler effects.
It does not measure a pure codec price or exclude repeated encoding/decoding as a contributing
mechanism. Copy/header evidence is not added/subtracted from this representation-choice result.

Split/bin complete in11.29s/4.93s under ordinary commands, without tracing/SVG/persistence or
saved-result refresh. The later split finishing below15 seconds does not establish the cause of
the earlier empty timeout; startup/cache/load explanations remain unproven. The short arbitrary
cutoff was not an instruction measurement or a semantic verdict. Successful artifact SHA remains
`2cec6974340e524bc3dfeec01e0ac2609d5ae53690f0c8532a1bbc9fe3692be2`.

`W/analysis.md`/`verification.json`/`final-checkpoint.json` bind result and retained source/target/
results/status/index/paths plus all old report sections and plans/evidence. Final plan structural
validation, references and whitespace pass. No production candidate, retained Rust/API/layout or
saved-result change, rebuild/native/lint rerun, stage/commit/push or persisted benchmark update.
The previous native34 and touched-format passes remain; scoped Clippy41 still fails and scratch-OOM
injection remains a production prerequisite. Historical708/Clippy40/39/workspace format/independent
review and0388 criteria2/7 remain unchanged. Diagnostic completion does not waive those gates or
approve the slower representation; overall performance HOLD remains.

## Validation

Worktree: `/Users/yota/dev/gleaph-spill`. [Current0393 closure](#validation-closure-0393) records the
actual final lint/runtime/format/review and document gates; acceptance itself supplied no pass.
Rows below preserve the earlier final results and old performance HOLD label on their exact nine
Rust hashes. Those historical failed/incomplete executions stay failed/incomplete; separate current
results close the required segment gates.

### Historical validation results

| Command | Final result and transcript |
| --- | --- |
| `cargo test -p ic-stable-lara --lib labeled::bucket_store::tests:: -- --nocapture` | PASS 17/17; `segment-prior-final-owner.log` |
| `cargo test -p ic-stable-lara --lib lara::edge::free_span::tests:: -- --nocapture` | PASS 34/34; `segment-prior-final-allocator.log` |
| `cargo test -p ic-stable-lara --lib` | PASS 708/708; `segment-prior-final-crate.log` |
| `cargo clippy -p ic-stable-lara --lib --tests -- -D warnings` | FAIL; `segment-prior-final-clippy.log`; same41 baseline messages/locations as v4, no new diagnostic |
| `cargo fmt --all -- --check` | FAIL; `segment-prior-final-workspace-format.log`; preserved unrelated formatting |
| `rustfmt --edition 2024 --check --config skip_children=true <nine exact owned Rust paths>` | PASS; `segment-prior-final-scoped-format.log`; exact arguments are bound in the ledger/manifest |
| `canbench bench_r_bs_segment_rw_` / `bench_r_fs_tkbfs_` / `bench_r_fs_lbin_` | PASS; seven cases in the final cost transcripts above; performance disposition HOLD; no persistence |
| Corrected source/diagnostic/test closure | PASS; `segment-prior-source-closure.log`, `prior-values-inline/closure.json`; prior correction receipts preserved |
| Independent test-only retention review | INCOMPLETE; `segment-prior-test-review.log`, exit124 at100s waiting on delegated review; no verdict, no retry |
| Preservation checkpoint and `git diff --check` | PASS; `segment-prior-preservation-v6-final.log`, `checkpoint-v6-final.json`; v4 production restored, one new test, all earlier receipts preserved |
| `python3 /Users/yota/.agents/skills/plan/scripts/validate_plan.py plans/0386-bucket-segment-publication-preflight.md --phase final` | FAIL: pending `independent-review-corrections` / `validation-and-design` and open criteria5/6; `segment-prior-plan-final-closure.log` records the exact errors |

Review checks the ready publisher, deletion of duplicate paths, original test-body preservation and
exact diagnostic mapping. The Rust production-prefix delta is +436 lines; this includes fixture-only
helpers before the test module. Private source/plan/preparation/ready values own staged proofs at the
existing descriptor, allocator, slab and vertex stores. No new signature has five or more meaningful
parameters. No new module, dependency, persistence schema, layout variant or public API was added.
Encapsulation and invariant enforcement remain with those stores; Graph callers consume capabilities
without importing allocation or encoding rules. One publication path maintains canonical/index/cache
consistency. A generic transaction layer was unnecessary; the alternative late-release `expect`
would still permit native partial mutation.

Clippy and workspace formatting were not waived:0393 closes them with real passing commands and
independent review. The residual scalar cost stays user-accepted under
[Performance acceptance](#performance-acceptance), without a necessity proof. Earlier final-plan and
draft-mode failures remain historical; the separate current final0386/0393 validators pass. Full
workspace tests, PocketIC, persisted benchmark refresh and unchanged0383 acceptance fixtures were
not run for this descriptor-only slice. Leaf/queue/Graph admission and the live-width decision remain
later dependency-ordered work.
