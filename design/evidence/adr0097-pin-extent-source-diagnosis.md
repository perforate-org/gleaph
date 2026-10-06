# Pin extent corruption during retained bypass conversion

**Status: Implemented and validated by0396. Mode prerequisite resolved;0394 geometry still requires a fresh baseline.**

The archived0394 restart P1 probe traced its pin/count conflict to the normal/bypass writers. Its
pin had16 slots at256. Both conversions left capacity272 and prefix5 unchanged but mutated the
count used as pin length; the return conversion published normal cover69 including64 spill rows.
The diagnosis and its RED below remain historical evidence, not current writer behavior.

## Reached state transitions

A temporary diagnostic in the existing
`labeled::graph::promote::spill_test::public_capacity_bucket_reservation_failure_preserves_cached_peer`
records the following source before constructing its second vertex. No production code changes.

| Stage | Mode | Vertex `stored_slots` | Slab prefix | Used spill rows | PMA `actual` | PMA `total` |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| After67 inserts | normal | 5 | 5 | 62 | 67 | 16 |
| After `enable_default_edge_bypass` | bypass | 67 | 5 | 62 | 67 | 11 |
| After filling the run to64 | bypass | 69 | 5 | 64 | 69 | 11 |
| After `promote_bypass_to_bucket_mode` | normal | 69 | 5 | 64 | 69 | 80 |

Every stage has edge base256, pin start256 and edge capacity272. Neither conversion allocates
edge slots; its allocation work is for bucket descriptors. The arithmetic is exact:

```text
normal -> bypass: total16 - old normal cover5 =11
bypass -> normal: total11 + max(segment16, bypass stored_slots69) =80
bypass stored_slots69 = initialized prefix5 + used spill64
inferred pin end =256 +80 =336 > edge capacity272
```

The diagnostic's explicit end/capacity assertion fails, exit101, exactly1 test. The pin has not
secretly grown. The three P1 possibilities are now resolved for this fixture: the spill-width alias
and count-floor mutation are both confirmed; an unrecorded physical expansion is excluded.

## Existing representations

The [segment physical contract](../storage/lara.md#3-segment-physical-contract-rope) defines the
assigned leaf interval using span start and leaf `total`. `SegmentSpanMeta` currently persists
only an8-byte start. The counts row persists `actual` and `total` as two `i64`s. These formats can
represent the valid source here: start256/total16, together with normal cover5 or bypass width69.
No additional durable pin-length field is needed to explain or repair this case.

`CsrVertex::initialized_slab_slots` projects a core/bypass stored width by subtracting used spill
rows. `EdgeStore::initialized_slab_slots` validates the run and owns that projection;
`bypass_bucket_view` already returns the correct slab descriptor width5. Normal bucket cover is
physical LEG geometry. `compact::bucket_resident_region` owns the slab/combined-root/tiny
projection. Neither bypass raw width69 nor tree logical extent is an allocated LEG cover.

`record.rs` now documents bypass `stored_slots` as initialized prefix plus used spill rows, not an
allocated cover. Its type/field/trait comments agree with the existing initialized-prefix projection
and reached69=5+64 state. The representation and persisted widths are unchanged.

## Entry owning writes and symmetric paths

| Site | Entry action | Ownership correction |
| --- | --- | --- |
| `graph/insert.rs::enable_default_edge_bypass` | Subtracts the normal vertex cover after publishing the bypass row | Preserve an assigned leaf width during a metadata-only transfer; prepare any unpinned contribution change before publication |
| `graph/bypass.rs::promote_bypass_to_bucket_mode` | Uses raw bypass width for `new_alloc`, then adds it to `total` | Obtain physical cover from the existing prefix/resident owner and use the same prepared count projection |
| `graph/promote.rs::promote_bypass_with_edge` | Uses physical combined-root width, but independently adds it to `total`; transcription later subtracts `actual` | Share mode-transfer count admission; preserve the distinct tree live-count exclusion and existing root allocation/retirement obligations |
| `graph/compact.rs::prepare_physical_layout` | Retains counts for pinned vertex rewrites; projects old/new cover for unpinned rewrites | Centralize that pinned/unpinned count decision with the mode writers, without loosening physical-only ready restrictions |

The tree-append sibling is a source finding, not another measured roundtrip. Its root comes from
a real standalone allocation. Treating that allocation as a leaf-pin resize would require an
allocation/placement proof it does not provide. The repair must not invent root containment,
retirement ownership or whole-operation atomicity.

Readers repeat the pin inference in `leaf_pin::labeled_leaf_physical_range`,
`compact::PhysicalLayoutSource::pinned_range` and four assigned-pin branches in
`EdgeStore::slab_window_exclusive_end`. Consolidate checked assigned-range interpretation at
`EdgeStore`, preserving row-window policy and the direct-scan contract. The strict capacity check
is necessary. Clamping80 to the available tail16 or resetting a corrupt count at a reader would
hide these writer defects.

## Why the entry tests missed it

The existing success fixture protects descriptor reservation failure and its cached peer. It does
not assert pin extent or physical normal cover during the earlier mode transitions.
`assert_labeled_edge_store_pma_counts` checks `actual`, but skips `total` when any bypass row is in
the leaf. That skips the intermediate11. Its normal-only sum/floor calculation is not an independent
oracle for arbitrary expanded mixed-mode pin widths. Tests should freeze the assigned interval
from allocation and compare it across mode transfers, including every intermediate state.

## Proposed prerequisite boundary

This section records the historical R1 proposal, implemented by0396 as described below.
Repair the physical-footprint handoff at the existing graph mode-conversion boundary, before0394
geometry integration. Keep current storage formats and descriptor publication owner.

1. Ordinary bypass→normal transfers the existing initialized prefix and run identity. Publish an
   exact physical cover from the resident owner, with no new unbacked quota. In this fixture the
   normal cover becomes5, the run stays64 and all69 logical edges remain.
2. Have `EdgeStore` select one prepared count projection for physical cover transfers. An assigned
   pin keeps its admitted width unless an allocator-backed leaf expansion/relocation changes it.
   Unpinned cover contributions use checked old/new physical widths. Keep `actual` changes explicit:
   the ordinary roundtrip leaves69 unchanged; tree publication retains its live-count exclusion.
   The low counts store owns arithmetic, encoding and row backing. Graph does not copy its schema.
3. Compose count/source admission with the existing descriptor/vertex ready publication. Validate
   indexes, arithmetic and witnesses before clearing or rewriting a row. An index precheck alone
   does not admit `checked_delta`; no recoverable count work follows the first canonical write.
   Extend the existing segment rewrite preparation where the final bypass row must be composed;
   do not publish an empty normal row as an intermediate conversion.
4. Delete the independent positive/negative total rules at the sites above. Reuse one checked
   assigned-range source for maintenance and row-window consumers. Do not add a reader fallback,
   mode-blind width helper, new metadata store, migration or scratch-driven repair cursor.
5. Require fresh state for experiments carrying counts/covers produced by the defective writers.
   Do not rebuild corrupt prior state to make this pre-production change appear compatible.

This proposal changes internal admission, not public/Candid APIs, MemoryIds or persisted row widths.
It needs a separately bounded prerequisite implementation plan/review. Q1 did not allocate these
mode writers. No production implementation is authorized by this diagnosis document. The new plan
must inspect mode eligibility at `enable_default_edge_bypass` before treating other tiny/tree/value
representations as a reversible slab/spill transfer; this fixture establishes only the latter.
Keep root placement, property/schema transformations and outer retries outside the shared count API.
No new module or boolean mode framework is proposed. Existing prefix/resident, prepared counts and
segment rewrite concepts fit the responsibility; obsolete raw cover/total rules must be removed.
Actual-delta variants stay explicit because ordinary and tree transfers have different semantics.

### Required validation for that prerequisite

- The reached slab/spill roundtrip, with exact prefix5/run64,69 targets in both orders, normal cover5,
  unchanged start256/width16/capacity272, aggregate counts, warmed peers and native reopen.
- Both ordinary conversion directions on a mixed leaf; retained pin slack must not be charged twice.
- An unpinned source, checking exact physical contribution instead of copied logical/run width.
- The tree-append sibling, independently checking physical root width, retained pin width, actual
  exclusion and source/destination retirement. If safe placement needs a separate allocator/outer
  contract, stop and split that prerequisite; do not discard the existing success contract.
- Count-index and arithmetic/backing rejection before descriptor, vertex, cache or allocator writes.
- Existing descriptor17/allocator34 tests, seven0395 identities and all current native identities;
  freeze any additional test/constructor/reopen budget from the written source.
- Mutation witnesses for charging a pinned cover and copying raw spill-inclusive width, then strict
  scoped Clippy/format and relevant canbench. Export names remain19997/20000; no new method.

After closure,0394 freezes the new native baseline and adds its13 geometry identities. Its original
715/728 inventory remains a historical Q1 estimate. The corrected K4 BEFORE and0395 cost acceptance
keep their original source bindings; prerequisite costs need their own measurement/disposition.

## Alternatives and review

A reader clamp or an already-pinned unchecked fast path is a small diff that preserves corrupt
writers. It cannot satisfy allocated-range ownership or native error contracts and is rejected.

Persisting an additional pin length is representable only after changing the8-byte span record,
its encoder, initialization and witnesses. It adds a synchronization obligation alongside counts.
The measured defect comes from invalid mode deltas and a logical/physical width alias, so that
format change is not selected.

Forcing every mode transition through whole-leaf relocation would combine logical handoff with
physical allocation and outer retry policy. It is broader than the observed metadata-only transfer
and is not selected.

Author recommendation: APPROVE WITH CHANGES for preparing a bounded prerequisite draft, subject
to mode eligibility and tree placement/retirement resolution. This is not independent draft or
implementation approval.

| Review axis | Assessment |
| --- | --- |
| Encapsulation | Stores retain encoding/backing and Graph composes final mode/descriptor publication |
| Separation of concerns | Logical mode handoff stays in Graph; range/count arithmetic stays in EdgeStore; GQL/Router are unchanged |
| Invariants | Source/count admission precedes destructive writes; an assigned width changes only with allocator-backed physical publication |
| Consistency | One prepared projection replaces independent positive/negative mode rules and shared checked range replaces reader formulas |
| Fitness for purpose | Existing prefix/resident/count/rewrite owners fit this measured transfer without a new storage format or framework |

The main risk is accidentally changing tree placement/retirement while sharing count admission;
those obligations need the explicit sibling test above. No new ADR is proposed because the repair
restores the existing segment contract; a changed persisted layout or allocation policy would
require one.

## Historical prerequisite draft and test-first entry

[Plan0396](../../plans/0396-mode-footprint-handoff.md) now bounds the existing-owner repair.
Its r3 independent packet-only review returns APPROVE DRAFT BOUNDARY, with one completed assistant
turn and zero tools. Earlier required corrections remain recorded: count backing before source reads,
zero-live versus prefix/run ownership, an actual allocator-capacity-growth witness, and separate
ordinary rejection preservation from allocated tree-target cleanup. The plan's Failure scope is the
single proposed rejection contract; it does not claim byte-identical rollback of tree allocation.
This is draft-boundary approval only. The writer/range/count repair is still unimplemented.

One new partial native contract, `mode_footprint_retained_roundtrip_preserves_assigned_pin`, reuses
one outer fixture/33 memories, with no reopen/PocketIC/install. Its RED reaches bypass69/total11,
normal69/total80, then bypass69/total11; required totals are16 in all three and normal cover5.
The exact69 rows in both orders, prefix5/run64/actual69, edge header and free intervals pass before
this assertion. Unlike R1's temporary probe, this named RED remains in the test module for repair.
The old715 identities pass with only this new RED excluded; scoped lib/tests Clippy and format pass.
This is not all-tests GREEN. The other seven planned identities and empty/mixed/unassigned/reopen,
backing/rejection/tree/mutation/performance checks remain pending. No old assertion is weakened.

`geometry-copy-composition/mode-footprint-p1/{entry-manifest,first-fixture}.json`,
`first-fixture-{source.rs,patch}` and `draft-review-r3-{report.md,completion.json}` bind the draft and
partial fixture. `mode-footprint-p1-roundtrip-red.log` records exact1/exit101/10.91s;
`mode-footprint-p1-old-native-contracts.log` records715/0/1 filtered. Production Rust, saved results,
protected receipts and0395 closure are unchanged;0394 remains stopped until0396 closes.

## Historical P2 continuation preflight

At P2,0396 continuation froze the preceding checkpoint before any new Rust edit. Its locked BEFORE
Wasm build fails before compilation because the workspace lock requires update. Locked metadata
also fails. Offline resolution in a separate diagnostic mirror identifies workspace drift, including
morph-dict's missing unicode-normalization dependency and obsolete/disambiguated explorer/GPUI lock
rows. A diagnostic one-row morph-dict update still fails locked metadata; no minimal real-lock repair
is proven. The canonical lock and all Rust remain byte-identical to the P1 checkpoint.

[The workspace lock gap](../implementation-gaps.md#gap-workspace-lock-drift--locked-build-prerequisite-resolved)
owned this prerequisite, then outside0396's approved file surface. Fresh BEFORE and further production
work were blocked at P2; neither diagnostic resolution nor retained old Wasm substituted for gates. `mode-footprint-p2/{baseline,lock-diagnostic,lock-delta-summary,minimal-lock-diagnostic}.json`
and `mode-footprint-p2-before-wasm.log` bind the stop. The first retained RED remains unchanged;
715-pass/Clippy/format evidence above is P1 evidence, not re-executed P2 validation.

## P3 lock prerequisite and fresh BEFORE

User `いいよ` separately authorizes only Cargo.lock alignment, with unchanged manifests/source and
no version upgrade or stage/commit/push. Packet-only r3 review returns APPROVE LOCK ALIGNMENT
BOUNDARY (exit0/13.50s, one completed turn/ZERO tools). Deterministic complete inventory and indexed
closure evidence dispose earlier objections without changing the candidate or old reports.
Real offline Cargo metadata exactly reproduces reviewed SHA256
`f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`; every retained version/source/checksum
and the optional-inclusive168-package LARA closure are equal. The lock delta has144 removals/zero
additions, six obsolete optional-edge reductions and the existing morph-dict manifest dependency.

Locked target metadata passes, then the original locked Wasm build passes (128.76s). Fresh BEFORE
instructions are `vls64`1531031, `vlt64`1578135, `tiny_promote_d4_1024`190825723 and
`bench_l_def_bp_iter_128`18854, all0 heap/stable growth pages. The fresh binary/export/source/CSV
bindings are `mode-footprint-p3-lock/{before-source,before-wasm,before-results}.json`, with19997/20000
export-name bytes. External evidence-only configs measure that frozen newly compiled binary;
no repository saved results are persisted and historical measurements are not relabeled.

The old runner's original/common-lock guard exits125 after the authorized generation child exits0;
its failure remains recorded. The current0396 runner deletes old-lock borrowing from its executing
contract. A receipt-key archive failure also remains failed, with frozen outputs verified before
completion. Original runners and protected evidence stay unchanged.
Current-lock native715 identities pass with only the deliberate partial RED excluded (8.95s).
Exact1 RED again reaches totals11→80→11/normal cover69 at the intended assertion (1.52s).
No all-tests GREEN, count/range/tree repair, AFTER, mutation or implementation approval is claimed.
All0396 TODOs/criteria remain pending and0394 remains stopped.

## P4 implementation evidence

The reviewed count/source/range boundary is implemented at the existing owners. One checked source
read admits count backing before reads; EdgeStore selects retained assignment or unassigned physical
contribution. Ordinary conversions and compact no longer own competing total rules. The descriptor
publisher admits a final bypass row directly. Tree transcription prepares actual exclusion before
allocation, binds the exact allocator-grown header and revalidates before publication; standalone
root allocation does not resize the pin. Rejection preserves ordinary canonical bytes or performs
the plan's owned-target tree cleanup, not byte-identical allocation rollback. Fresh state is required
for previously corrupted counts/covers; no format/migration/reader clamp is added.

`geometry-copy-composition/mode-footprint-p4/{fixtures-r4,native-validated}.json` freezes16 fresh graphs,
1 outer graph,290 memories,8 native reopens,0 PocketIC/installs, and all723 passing identities. Coverage
includes exact retained cover/pin, mixed peer/slack, unassigned physical contribution, empty/zero-live
and valid empty-run ownership, count index/arithmetic/backing, stale witnesses, eligibility and real
tree allocator-capacity growth/source retirement/cleanup. Three separate mutations reintroduce
assigned charging, raw spill-inclusive cover and late count admission; each exact test fails, and
byte-identical restoration plus the same test passes. The native723 gate preserves all715 identities.
Strict lib/tests Clippy, format and locked Wasm build pass. Earlier failures remain failed.

Independent packet-only review returns REQUEST CHANGES (148.22s, one completed turn/ZERO tools).
P1 requires preparation before final source/count/header admission; P2 requires exact free LTB target
IDs/integrity and source logical-row/allocator verification on reopen. `mode-footprint-p4/review-
{report.md,completion.json}` binds that original candidate; its findings are not relabeled as approval.

P5 user-authorized continuation removes both fallible callbacks. Private transient prepared/ready
sum types retain the existing normal image or bypass segment writer; one transcriber check follows
all storage preparation, immediately before infallible publication. The same6 tree cases retain active
source LTB0 and check exact free targets1/2, links/kinds/terminator and original source header/payload.
The count case injects during descriptor backing reservation, and existing rejection reopen verifies
exact logical rows both orders plus layout/allocator integrity. The bypass audit consumes initialized
prefix from EdgeStore rather than treating spill-inclusive width as physical. Budget remains290/8.
Native723/strict Clippy/format pass. Moving admission before reserve again fails the advertised exact
error assertion; byte-identical restoration and exact GREEN pass. No new persisted layout/scan rule.

A fresh hash-bound runner derives the old immutable real-lock contract and admits only R/crate/four
new AFTER directories, as explicitly authorized. Locked fresh Wasm and four frozen-CSV controls pass:
slab1530725/tree1577829 (each-306), tiny191032946 (+207223/+0.1086%), bypass18854 (unchanged), page growth0.
Independent re-review accepts the small tiny pressure-retry cost as bounded safety-admission cost,
not exact attribution, statistical noise or borrowed0395 approval. Sanity/cardinality/measurement is unchanged.
`mode-footprint-p5/{baseline,runner-permission,native-validated,mutation-restored,after-results}.json`
binds source/protection/runtime/cost. P5 packet-only review returns APPROVE IMPLEMENTATION AND COST
BOUNDARY (45.80s, one completed turn/ZERO tools), no remaining concrete P1/P2. `mode-footprint-p5/
{review-completion,pre-closure,closure}.json` binds that verdict and final real-plan/source/protection/
index gates at the pre-commit checkpoint. The subsequently user-authorized scoped commit and its
index-only source/validation are bound by `mode-footprint-user-commit-c1/commit-receipt.json`;
no push is authorized.0394 requires a fresh723-native source/performance baseline before geometry
work; old0395/BEFORE/RED and historical715/728 bindings remain unchanged.

## Historical R1 evidence

Evidence root:
`/Users/yota/.local/share/gleaph/evidence/adr0097/maintenance-fixtures-f2-f4-0383/segment-implementation/tiny-leaf-geometry/geometry-copy-composition/pin-extent-source-r1/`.

`baseline.json` freezes source, documents, protected P1/0395 receipts and retained Wasm.
`stage-cause.json`, `stage-red-source.rs` and `stage-red.patch` bind the exact diagnostic source and
four reached states. `pin-extent-source-r1-stage-red.log` in the maintenance evidence root records
its one-test failure. One existing outer fixture allocates33 native memories; no extra constructor,
PocketIC, canister install or reopen is introduced. Temporary diagnostics are archived and the
source is restored exactly. R1 runtime evidence establishes the source defect only; it is not
writer-repair, all-mode,0394 GREEN or performance evidence. P4 evidence is identified separately above.
