# ADR0097 — Demotion/deletion design prerequisite

**Status: Experimental — retained current source R; scoped local-position checks complete; all six original IC ceilings unmet; performance and ADR0097 acceptance BLOCKED.**
**Historical opening — the shared-reader `5da46e45…` checkpoint, not the current map checkpoint.**
After review of report `01b6d0dd`/plan `ecb7968e…`, the user authorized one reader candidate and one
new helper oracle, not engine/performance acceptance. In that checkpoint source `5da46e45…` passed
final18/27/5 and all four runtime sensitivities; the [execution checkpoint](#shared-reader-execution-checkpoint)
records lower missing/replacement cost and the emitted bypass, but **every original IC ceiling still
fails**. The fresh222.562308/600s ledger is closed; no second candidate, tuning, baseline
rebuild/restoration, broader execution, commit or merge occurred **in that checkpoint**. The reader,
M/instrumented, tooling-preflight and pending remaining-validation prose below are preserved
historical records, not present status; current status is the
[frozen-R execution checkpoint](#frozen-r-execution-checkpoint). Q/W/E/D/C remain historical;
full A0/A1 and ADR0097 landing remain unapproved.

The earlier [caller-owned-image correction](#caller-owned-image-correction-checkpoint),
[original execution](#map-owner-execution-checkpoint), [source/static-Wasm diagnosis](#source-and-wasm-diagnosis)
and [remaining-read-path diagnosis](#remaining-prospective-read-path) retain their historical source
binding, snapshots, receipts and refusals. Independent review `4eb344e9`/source `9436291c…` found no
new correctness defect in that bounded ownership/codec delta and satisfied those corrections, not
the cost gates. The subsequent source-only diagnosis proposed the now-executed guard; its lack of
new runtime evidence describes that preserved checkpoint, not the new execution below.

The user previously conveyed **APPROVE for the bounded admission repair only** (complete review
`bdae4ec2` and its watch log). Its two source images below remain preserved as the admission baseline;
FreeSpan is still byte-frozen in the checkout, while the new map candidate is separately identified.
That scalar lookup/nonwriting reservation trade-off is not a waiver for the new engine, full A0/A1
or landing. The reported admission review was read-only and was not rerun here.

Admission execution is closed at `$TMPDIR/spill-admission-f297-6g1p_any/`. Plan0339, its executed
snapshot/check, ledger and manifests retain their original contents, including historical
review-pending fields. The new acceptance is recorded here with separate prose-check provenance.
Commands in that execution were serialized; tests used normal scheduling, without thread overrides.
No unused balance is reopened. Full A0, A1, reclamation and native deletion remain unimplemented.
Review `e9e4e39f` accepted the nonpublishing direction for **further design only** and requested P2
changes to Draft0340's scalar cost gate. The historical prose revision is preserved below; its
subsequently authorized map-only execution does not implement the span protocol.

**Acquisition decision anchor:** 2026-09-22 17:46:18 UTC +0000 (local UTC observation).
The current decision is specified in ADR0088; the superseded r2 sketches remain in preserved history.
**Preserved interface anchor:** 2026-09-22 14:40:03 UTC +0000.
**Preserved r1 anchor:** 2026-09-22 13:17:45 UTC +0000 (original draft: 12:00:21).
**Inspected base:** `f297706abf683c1e22622c193d34f06650305bee`, branch `spill-swap`.
The reduced implementation is an uncommitted two-file patch on that base, identified below.

The independent review approved GAP006's bounded selected-property read slice through this base.
It did not approve deletion, demotion, matching, general atomicity/performance or landing. The
initial comparison main was `9a64049f47fc07542446d651d4b09421636b73c5`. The final prose check
observed shared main advance to `8b63042c68e5981e3c7afa04c2d3660485e5ce16`; this checkout's HEAD
and approved Rust hashes stayed fixed. No ref reset, commit or merge was performed here. The failed
reference-pin check is retained, not converted to an unchanged-main claim. Historical evidence stays
bound to its original refs. Subsequent reconfirmation uses immutable `8b63042c6` (lock-only, matching
preserved as-used `f5d323d0…`) and `6d82d0fc64a8a44bbd424efd7553ea6a347eee4a` (Router/docs changes,
not these owners). The map execution pinned these immutable references and its own source images
without querying or repinning shared main. HEAD stays f297 and checkout lock `15d08391…` is restored
after every invocation; integration remains later.

The decision belongs to [ADR0088: Planned position-preserving demotion](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#planned-position-preserving-demotion-and-deletion),
not this audit. The revision withdraws permanent nonempty retention as a chosen policy, separates
spill-free admission from existing prefix/run representability, and requires preparation of the
whole outer publication unit. Preserving 512/1024 thresholds is not proof of unchanged shrink
behavior. Neither this revision nor Stage A approval would approve permanent retention.

## Frozen-R execution checkpoint

**Retained current source: R `d2980db54ca03b4893b7f6031ec5a6147927e345074250fe3ed8ab733c21298d`; scoped local-position checks complete; performance and ADR0097 acceptance BLOCKED.** R is Q plus five reviewed replacements. `EditPlan::apply_edit` forwards its full-page absent-key `Err(pos)` to the existing `split_page`, removing only the repeated search. The position0..=64 belongs to the same prospective image with no intervening write. Split admission, `DirectoryFull`-before-directory-scan, active/full/header/count checks and publication remain at their existing owners. The cfg adapter forwards fixture position64 without a duplicate precheck. All21 test functions remain; only that fixture call changes. No new public API/type/cache/allocator/publisher/layout or context handoff.

**Original execution evidence, not a new campaign.** The positive R run passed21/0failed/0filtered and formatting. Of42 records (5 scalar,33 image,4 cover), only split reads60→53 and bytes1344→1288 changed; other work/scratch/write/image/cover fields matched Q. One key1-only fault (`1a5bcd36…`) preserved seed1000/position64 but failed the exact98-pair assertion at `lib.rs:2202:9`: keys0,2,1,4 instead of0,1,2,4, with unchanged values and five work records. Later metadata assertions were unreached; this is one fault/one test, not a middle-position discriminator. Scoped whitespace/map lib+tests+canbench clippy and FreeSpan27 passed (652 filtered; same27 Q names). This is not LARA lint:15 test-code and4 canbench-code warnings remain. Formatting was not repeated in final controls. E's eight/six sensitivities and Q's two/two retain their own epochs.

| IC gate | R | Q | Original ceiling |
| --- | ---: | ---: | ---: |
| scalar split | 69444 | 70497 | 30930 |
| scalar missing | 128296 | 128296 | 85672 |
| scalar replace | 149000 | 149000 | 77576 |
| scalar tail | 561354 | 561354 | 298171 |
| LARA total | 2147387 | 2147387 | 1527576 |
| LARA named split | 2146426 | 2146426 | 1526615 |

All six Q-relative conditions pass (split strictly improves; others do not regress); **all six original ceilings fail**. Scalar aggregate913027 (Q914080) is reference-only. No tolerance, averaging, aggregate or budget waiver, per-read IC price, allocation/peak-memory, codegen or full-suite claim follows. Raw canbench display rounds counts and compares with saved YAML, not Q; its `+41.34%/+41.37%` is not an R−Q delta.

**Post-reboot evidence.** Original trial roots were `$TMPDIR/spill-position-{native-3c1218f9,sensitivity-10d67f30,scalar-fa2501ec,lara-62228f64,final-3a10e7bf}`; they are now absent. Exact retained text and surviving binaries were copied with hash comparison to `~/.local/share/gleaph/evidence/adr0097/recovery-R-eda0b174/` (23 selected files, not a full archive/seal). The positive native2284496B/`92e44ab9…` binary remains missing, although its exact stdout/stderr and source binding survive. The current/copied map native2284496B/`e44583d9…` is the intentional fault, not correct R. Map Wasm704575B/`afe805fc…`, LARA Wasm5034468B/`98019392…`, and FreeSpan native39471200B/`04cc0aa2…` retain their R bindings. Validation trials restored source Q, not target artifacts; retention does not relabel those epochs. Used lock`f5d323d0…` differs from checkout`15d08391…`. Original R CSVs, full prior manifests and full FreeSpan stdout were not recovered; content-identical Q CSV is not R provenance.

Selected exports are the original [source patch](sources/adr0097-map-split-position-R-source.patch), [scalar measurement-command output](sources/adr0097-map-split-position-R-scalar.stdout), [LARA measurement-command output](sources/adr0097-map-split-position-R-allocator.stdout), and [fresh final-plan refusal](sources/adr0097-map-split-position-R-plan-final.txt). The two measurement outputs preserve exact integers and original comparisons from Pi tool records `e75ddbf8`/`13384308`, not rounded canbench display or regenerated CSV. The refusal is actual fresh stderr: exit1 for pending `bounded-proof` and unchecked criteria5/6, not copied Q evidence. A fresh post-reboot pre-edit snapshot protects this patch only; it cannot prove the lost historical manifests unchanged.

**History and limits.** The first R proposal's duplicate guard/wrong seed/coverage was rejected;1304B was corrected to1288 before execution. Min5 without a split is not front-split proof, near-front1 is not0, and seven reads are not saved uniformly on every staged split. Numeric-timeout-string exceptions, report-cap overages and Q retention's HEAD-predicate exception remain history. Recovery inventory `b8a6eebb` timed out and was aborted; later selected recovery succeeded. The post-delivery coordinator NameError and proposal247/240-word overrun remain recorded, not repaired by replay. Retention SETUP `70a6f238` failed before any root/repository write: the reviewer wrongly validated a partial replacement as a complete line. An unreached transcription also changed `out.fileno()` to `f.fileno()`, so its report's “exact call” claim was false. That grant ended without retry. Shell receipts5/4/17/76/12s are not aggregate-budget proof. Full A0/A1, mutation, ownership/ordinals/movement/sidecars, demotion/deletion/matching, shrink/reclamation/retirement, plan0340 criteria5/6, historical600/120, and ADR0097 acceptance/landing remain open.

## Frozen-Q execution checkpoint

**Historical Q checkpoint:** the following evidence and target-state statements describe the Q checkpoint, not current R. Its measurements, anchors, exports and archive bindings remain unchanged. Original temporary paths are historical; post-reboot loss and selected recovery are recorded above.

**Historical source: Q `51a32c32bfec364d3d3bba880f9e0327929ca9741805fc93326b1bbf429c8229`; scoped directory-reader checks complete, performance and acceptance BLOCKED.** Q adds the private `DirEntry::decode` method and `canonical_dir_entry_single_read_contract` to W. The existing `DirEntry` type owns directory decoding: canonical `read_dir_entry` changes from two 8 B backing reads to one 16 B read; prepared `directory` retains its 16 B prospective `EditImage` request. In-page entry decoding, encoding, locator/selected tuple, minimum guard, count, admission, publication, layout and borrow authority are unchanged. No second editor, cache, allocator or publication path is introduced.

Source-bound evidence:
- `$TMPDIR/spill-directory-green-93413c40`: 21 unique tests passed, zero failed; all 42 diagnostic records match W with multiplicity preserved: 5 `SCALAR_WORK`, 33 `IMAGE_READ_CASE`, 4 `FIRST_COVER_CASE`. Archived native: 2286768 B / `a7a040c6…`.
- `$TMPDIR/spill-directory-decode-d09859c2`: shared field-swap fault reaches the direct `DirEntry` equality failure at `lib.rs:2062:9`; later backing/get/Traffic assertions are unreached. This is not an endian mutant.
- `$TMPDIR/spill-directory-bypass-94b3042b`: canonical-only prepared-directory fault reaches the second-batch unwrap at `lib.rs:2343:45`, `Edit { at: 1, source: Corrupt }`; later checks are unreached. These are two fault invocations in two functions, not E's inherited campaign.
- `$TMPDIR/spill-directory-scalar-6e961e07`: missing128296, replace149000, tail561354, split70497, all equal to E. Original ceilings are exceeded by42624/71424/263183/39567 respectively. Aggregate914080 is reference-only.
- `$TMPDIR/spill-directory-lara-c0d06788`: total2147387 and named2146426, each37242 below E and619811 above its original ceiling; total-minus-named remains961. All six preregistered E-relative conditions pass; all six original ceilings fail. No tolerance or aggregate trade-off applies.
- `$TMPDIR/spill-directory-final-245830a7`: source formatting, scoped whitespace and map lib/tests/canbench clippy exit0; FreeSpan27 passes with the same names as E (652 filtered). Admission diagnostics are in stderr, not stdout; there is no `ADMISSION_PASS`. Clippy reports the same unused-config-key warning twice.

The validation attempts ended by restoring W; the later retention changes source, not artifact provenance. Configured map Wasm is Q, 704719 B / `35190068…`; configured LARA Wasm is Q, 5034763 B / `295aaca6…`. The debug map native remains the bypass mutant, source `41bc3bd9…` / artifact `08f902a9…`, not correct Q or W. The FreeSpan native is Q-bound, 39490816 B / `ce882aab…`. Actual paths, source/copy hashes, used locks and complete streams remain in the named roots. Source restoration never restores or relabels target artifacts.

Selected exports, not a full archive or seal: [Q source patch](sources/adr0097-map-directory-Q-source.patch), [Q scalar](sources/adr0097-map-directory-Q-scalar.csv), [Q allocator](sources/adr0097-map-directory-Q-allocator.csv), and [Q final-plan refusal](sources/adr0097-map-directory-Q-plan-final.txt). The refusal is fresh captured validator stderr, exit1 for `bounded-proof` pending and criteria5/6 unchecked, not a copied W/E receipt or plan completion. E's eight sensitivities and doctests remain E-bound; W's two cover faults remain W-bound. Separate slices do not demonstrate the original600s/120s aggregate contract. No per-read IC price, zero-allocation, peak/codegen equivalence, full-suite/all-name/PocketIC, full A0/A1, deletion/demotion/matching, shrink/reclamation/retirement or ADR0097 landing approval follows.

Q-era history: the directory-red attempt omitted the terminal LF in two supplied shell payloads; its intended read-granularity red remains valid, with that fidelity exception. Reviewer construction `540859c5` failed before delivery and issued no implementer command. Subsequent Q green/decoder/bypass/scalar/LARA/final payloads were byte-exact. Read-only documentation proposals `327be8a3` and `0275d96e` remained unapplied; the latter contained a nonmatching Context hunk and omitted ADR oldText. Preflight `69826b63` stopped at its first line-prefix assertion before emitting any payload; its later checker/body defects were unexecuted, not passes. Printed hashes, absence predicates and later actual hash comparisons remain distinct; no active-time cap is proved by their wall intervals. Prior W failures and evidence remain below.

## Frozen-W execution checkpoint

**Historical W checkpoint; test-only follow-up complete, not the current source.** Its retained source was
**W `3a6e3d27e7c61940254cb28aeae60726248faa0cb812053a59e7a5e96428b425`** — production and the19
old tests/helpers are **byte-identical to E `172879d5…`**; W adds only
`edit_image_cover_empty_entry_and_zero_count_contract`. **F's and G's production branches have been
REMOVED.** **E remains the last completed scoped IC/inherited-sensitivity/doc/FreeSpan campaign and
the performance reference — not the current full lib source.** The E/D/C sections retain their
source-bound results and artifacts. This checkpoint preserves W evidence; current status is owned by the R checkpoint above.

**Witness:** one one-page backing with injective128 bytes; three private images and four phases
(`inline-first`, `inline-disjoint`, `heap-first` — actual capacity ≥48 before `cover(…,48)` with
exact capacity equality afterward — and fresh-empty `empty-zero`), asserting callbacks, packed
offsets/descriptors/bytes and unchanged backing. No new map/Graph/PocketIC fixture. A **fresh-empty
count0 keeps one zero-length descriptor and no callback**; an **already-contained count0 takes the
existing contained return without adding a descriptor**; no orphan bytes, monotone-descriptor claim
or unwind/reuse guarantee.

**Evidence (source-bound roots; raw copies are selected evidence, not a full archive or seal):**
initial **20 unique tests /33 image cases / four phases / all5 COMPLETE E-identical work records**
at `$TMPDIR/spill-cover-witness-f483e684` (native **2283728**, sha `d8e2edbf…`); two **W-specific
faults of the one unchanged function** reaching the FIRST callback assertion — fill
`$TMPDIR/spill-W-cover-cb9aec28-fill` (**only** `inline-first`, `[]` vs `[(256, 8)]`) and zero
`$TMPDIR/spill-W-cover-cb9aec28-zero` (first three phases complete, **all4 markers**, `[(376, 0)]` vs
`[]`); the later failing-phase offset/descriptor/bytes/capacity/backing assertions are **unreached**
and there is **no independent Heap kill**; scoped format/lint at
`$TMPDIR/spill-W-final-58f5ec12-{format,map_clippy}` — **scoped lint/format complete; no extra W
runtime/doctest/FreeSpan gate, no W IC run, no binary relabeling.** Durable raw copies:
[W patch](sources/adr0097-map-first-cover-W-source.patch) ·
[F patch](sources/adr0097-map-first-cover-F-source.patch) ·
[F scalar](sources/adr0097-map-first-cover-F-scalar.csv) ·
[G patch](sources/adr0097-map-first-cover-G-source.patch) ·
[G scalar](sources/adr0097-map-first-cover-G-scalar.csv) ·
[plan refusal](sources/adr0097-map-first-cover-W-plan-final.txt) (expected exit1: exactly
`bounded-proof` pending and criteria5/6 — plan0340 is not completed).

**F/G are separate rejected candidates, not W evidence.** Each had its OWN20-test gate, two
fast-branch faults and one scalar IC measurement: F `6e0892aa…` places the positive-empty branch
before the original `ranges` binding; G `9857b512…` after the contained return, using `ranges`.
**Both rejected:** split **F71669 / G70740 > E70497 (+1172 / +243)** — no tolerance, averaging,
aggregate trade-off, failed-F control or third placement variant. Actual values:

| Candidate | replace | missing | tail | aggregate | E-relative split | LARA |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| F | 133192 | 128296 | 532426 | 870516 | **71669 FAIL (+1172)** | unmeasured |
| G | 134728 | 128296 | 534730 | 873427 | **70740 FAIL (+243)** | unmeasured |

The E-relative rules required a strict replacement improvement and no regression in missing,
tail or split. Both met3-of-4 conditions; **all four ORIGINAL scalar ceilings fail for each**; **both
LARA gates UNMEASURED on each**; aggregate is reference-only. **E's six original ceilings still
fail.** Zero heap/stable growth is not zero allocation; no per-check/frame/codegen pricing from
these totals; detailed E/original numbers remain owned by the existing E matrix.

**Identity/scope:** the W-positive native is **2283728 / d8e2edbf…**. F's native is
**2283920 / 573147d0…** (`$TMPDIR/spill-F-native-2be88a3a`); G's is **2283920 / 8c27f7ba…**
(`$TMPDIR/spill-G-native-6a0c5ef9`). F's scalar Wasm is **707449 / 57540000…**
(`$TMPDIR/spill-F-scalar-42f785a6`); G's is **707192 / 064c44e2…**
(`$TMPDIR/spill-G-scalar-90308710`). Their path/source/copy bindings remain in those roots;
**source restoration does not restore target binaries**.

**History preserved (not replayed):** the omitted cover SETUP/unexecuted draft and retracted success
claim (`$TMPDIR/spill-cover-stop-edbdd116`; old `ed25` root absent); the W-native semicolon/newline
drift and the **W-format terminal-LF omission (valid observation, not exact fidelity — separate from
the fatal G-park SyntaxError)**; F parked at `$TMPDIR/spill-F-park-dc41aadf`; G's first park left only
an empty root after a corrupted-assertion SyntaxError; `$TMPDIR/spill-G-park-4e8ebea8` actually restored
W. Old failures/roots/receipts immutable. **Partial integer samples and fresh budgets do NOT prove
historical600s/120s compliance.**

The first W documentation attempt (`$TMPDIR/spill-W-doc-7689da9d`) copied five exports and updated
this report, then stopped at a nonmatching plan edit; no plan/ADR edit or validator ran. Its setup
recorded seven expected protected hashes without checking them. Independent review subsequently
verified the before-images, immutable exports and foreign state; that is not a retroactive setup
check. A second attempt (`$TMPDIR/spill-W-doc-65ac6168`) verified its setup and applied eight plan
hunks, but sent the ninth as a no-op with an extra `newText2` key. It stopped without ADR/report
edits or validator execution. Both old roots' `receipt.txt` files are setup-only. The separate
remainder is recorded at `$TMPDIR/spill-W-doc-9a49a7db`; the sixth durable is the structural refusal,
not a fourth copy. Neither stopped attempt is retroactively completed by the remainder.

## Frozen-E execution checkpoint

**Production/performance reference checkpoint (E — full E source is NOT current); at its time: status Experimental, frozen-E functional/lint/doc controls complete within their scoped slices, performance BLOCKED.** Frozen E = map lib
`172879d5e99e3b914a1e32687c4e08c728abca689ac3cf653cdf48e5385f7acf` = frozen D `da816bfa…` plus
**two additive test-relevant changes only**: one valid-backed three-key minimum-corruption unit test
(`prepared_edits_reject_inconsistent_minimum`) and the local `i == 0 { dir.min_key }` reuse of the
just-validated first key inside `apply_edit`'s existing `search_page` closure (the required
first-key equality guard remains; every intent selects a fresh prospective directory; there is no
intervening write between validation and search). No added binding, cache, state, API, layout or
owner; the existing count guard, split pre-covers, `PageHeader` codec, admission and consuming
publisher are unchanged. The new test **extends an already-correct D guard — it does not fix a D
bug**. All18 old tests and helpers, fixtures, benchmarks and FreeSpan remain byte-identical; the
added fixture cost is one additional unit map, zero PocketIC or installs.

**Measured gates (binding; every original gate FAILS):**

| Scope | E | D | original ceiling | outcome |
| --- | ---: | ---: | ---: | --- |
| `scalar_missing_64` | 128296 | 136872 | 85672 | FAIL +42624; **−8576 vs D (−6.265708106844351%)** |
| `scalar_replace_64` | 149000 | 151560 | 77576 | FAIL +71424; −2560 vs D |
| `scalar_tail_pairs_64` | 561354 | 562954 | 298171 | FAIL +263183; −1600 vs D |
| `scalar_split_directory` | 70497 | 70627 | 30930 | FAIL +39567; **−130 vs D** |
| scalar aggregate (reference only) | 914080 | 926946 | 497282 | reference, **not a seventh gate** |
| LARA total | 2184629 | 2185529 | 1527576 | FAIL +657053; −900 vs D |
| LARA named | 2183668 | 2184568 | 1526615 | FAIL +657053; −900 vs D |

**Six candidate-relative conditions are met:** missing and split strictly improve, while replace,
tail and both LARA rows are non-regressing (exact integers; no noise allowance, no changed
ceilings). **All six original gates FAIL.** Aggregate914080 vs D926946 vs original497282 is
reference ONLY — not a seventh or substitute gate. Native deltas: missing **11/120 →10/112** and
split **61/1352 →60/1344**; every other field of all five COMPLETE records is unchanged
(split max read **512**). These are **fixture-level measured benefits — not per-read or component
prices, not codegen or exact-IC-bias evidence, not a global-allocation proof**; zero page growth is
not zero allocation. Benefits do not waive any ceiling.

**Witness outcomes (eight sensitivity invocations across six test functions, all bound and
restored to E; positive runs are not independent kills of unreached assertions):**

- **Positive minimum witness:** poisoned directory minimum (10→9) with page1/ACTIVE/header/backing
  valid → public prepare rejects `PrepareError::Edit { at0, Corrupt }` and scalar remove returns
  `Corrupt`, both with byte-identical poisoned canonical bytes; restore → exact `previous_values`,
  discard-without-write, retry, publish, surviving pairs and `validate()`.
- **Guard negative:** the same poisoned prepare wrongly returns ready → `discard` → panic
  (`minimum mismatch unexpectedly reached ready`) **before** the byte-comparison, scalar-rejection
  and retry/publish assertions — those later phases did not execute.
- **Zero negative:** probe0 returns constant0; the fixture's real FREE-page rejection after staged
  edit0 still fires, then the healthy remove of page2 minimum128 fails the exact prior assertion
  (**`[Some(value64), None]` vs `[Some(value64), Some(value128)]`**); publication, final
  pairs/minimum/len/`validate()`, scalar-parity and AddressOverflow assertions were **unreached**.
- **count/resource:** runtime101 at the required `expected actual restricted grow3/+2 before ready`
  assertion — **not** an observed successful grow, no printed `Corrupt`, and not evidence that the
  actual intent3968 executed.
- **early:** `prepared_edits_directory_rejection` reaches the exact `late DirectoryFull changed backing`
  non-mutation assertion (lib.rs:2398:13, runtime101, one failed/18 filtered): the temporary
  early-publication fault changed canonical backing before the late `DirectoryFull` rejection.
- **disjoint:** exactly3 printed image cases, then `inline-contained` arrays `[19,20,21,22]` vs
  `[162,163,164,165]`; **backed:** OOB at the25th/last printed `unbacked-unstaged`.
- **Left-fill:** the fault is introduced at the seeding **pos64 split**; **ALL97 seed entries —
  including the32 inserted after that split — exist at the failed initial pair assertion (32 zero
  pairs, still97 rows)**; later fixture initializations, the timed scalar phases and the timed pos1
  split / `SCALAR_WORK` records were **not reached**.

Faults are counterexamples, not current E bugs.

**Source epochs and attempts (honest, separately bound):** T `4b2b7de…` — the reviewer-supplied
assert formatting failed `rustfmt` **before Cargo** (the independently observed checker-text drift
did **not** cause that format failure); fresh **T1 `97490252…`** passed format and the19-test gate;
the T1 guard-omission negative `2797bd…` remains a **separate T1** invocation, never relabeled E.
The E-LARA literal carried one receipt-wording deviation (`single shell through sample` →
`single shell through this sample`); its observations are valid and the compliance deviation is
retained, not repaired. Prior provider/schema/tooling/archive/budget failures remain history — see
their existing records rather than a recopied chronology.

**Evidence (retained local roots — local artifacts are NOT a complete durable export or seal):**
minimum-witness `$TMPDIR/spill-minimum-witness-da94bf18`; minimum-format
`$TMPDIR/spill-minimum-format-478ed621`; T1 guard-negative `$TMPDIR/spill-minimum-guard-a75508ca`;
initial E `$TMPDIR/spill-E-native-0f0a353b`; scalar `$TMPDIR/spill-E-scalar-cd729826`; LARA
`$TMPDIR/spill-E-lara-bb5b847d`; first-key negatives `$TMPDIR/spill-E-firstkey-0d271f2b-{guard,zero}`;
six inherited `$TMPDIR/spill-E-inherited-f1e94af5-{count,resource,early,disjoint,backed,left-fill}`;
six final controls `$TMPDIR/spill-E-final-48c5db75-{format,map_clippy,lara_clippy,map,free,doc}` —
with source and lock snapshots in the execution roots; explicit toolchain/version captures are
in the scalar and LARA IC roots. The failed T formatting attempt never reached Cargo, so its
checkout-lock capture is not an actual-used Cargo lock. Scoped finals: **19/27/5 (one E
five-doctest invocation),33 image cases, all5 record identity against the accepted initial-E
reference**; lint scopes map lib+tests+canbench and separate LARA lib-only with warnings retained
outside those scopes — **not** broad LARA/kernel/PocketIC/full acceptance. Artifacts: scalar Wasm
**704959** `0fa7b31053aefea3cfa204e5c4498ccba744cd729bcdc1cd2673b637349733ca`; LARA Wasm
**5035002** `9da05634c81ae36766dd519b597810b0f472a8b553b12f71e42cffc00d8c876f`; initial E map
native **2219392** `654c4eab033f0cfa397f4a4d135dbbb7c9466e16ac53e5f86e3361fc0cf5d357` vs
final-E map native **2219392** `9689227d8008becc0f93fe84628d42c8e1e66cdc46a7e6a96775d8663f9cede1`
(identities retained distinct — **not cause proof or codegen equivalence**), FreeSpan native
**39490368** `3cf3f0b456abad65b75ae674b1c191350cead64e63a93975376e1c5e099d0f92` (paths in each
root's `binary.json`). Durable copies: [scalar CSV](sources/adr0097-map-first-key-E-scalar.csv) ·
[LARA CSV](sources/adr0097-map-first-key-E-allocator.csv) ·
[combined D→E source/test delta](sources/adr0097-map-first-key-E-source.patch) (one combined
patch covering the test and the reuse — **not** a T1→E production-only patch) ·
[final refusal](sources/adr0097-map-first-key-E-plan-final.txt) (expected exit1: exactly
`bounded-proof` pending and criteria5/6 — plan0340 is not completed). No binary was recreated,
recopied after live overwrite, or exported wholesale.

**Documentation attempt and continuation:** `$TMPDIR/spill-E-doc-cfd23c4b` preserves the first
status-sync attempt. Its validator exited1 with exactly the three expected errors on stderr and
empty stdout. The reviewer-supplied checker wrongly required empty stderr and stopped before
exporting the refusal or recording scope completion. That failure and the partial documents are
not retroactively repaired. The fresh `$TMPDIR/spill-E-doc-8036ba1e` continuation corrects the
remaining prose and restores the original “single corrected-source” measurement qualifier in
criterion5; its new source-bound validator refusal and scope evidence are separate. No Rust or
IC validation is repeated, and none of these document checks closes the pending cost gates.

**Boundaries (SSOT):** the generic map owners (geometry, count, codec, admission, publication,
dirty-image data) remain the single sources of truth. This status slice confers **no** full A0
acquisition/capacity/heap/index/retirement authority and **no** A1 whole-unit publication proof.
ADR0097 ordinals/sidecars/ownership/error-order/shrink/reclamation/deletion/matching/demotion gaps
and landing all remain open; ADR0097 itself is unchanged. Fresh slices and integer samples do **not**
prove old600/120 compliance; every original ceiling stays independently binding; missing historical
artifacts and budget failures are kept unrepaired. This status task authorizes no next optimization.

## Frozen-D execution checkpoint

**Historical checkpoint — no longer current status; at its time: status Experimental, frozen-D functional/lint/doc controls complete within their scoped slices, performance BLOCKED.**
Frozen D = map lib
`da816bfae3f981c3e97144c4a0a9a2104239ae7ebdf5c00c1d672bce8648e960` — immutable frozen C `a9e1581d…`
(the shared-reader candidate plus the guarded `EditPlan::page` count reuse) plus **exactly13
production lines** that add two `EditImage::cover` calls after checked right-page allocation,
offset and alias checks and the `split` calculation, before the unchanged right-first/backwards-left
loops; benchmark `2f95a494…` and FreeSpan `35b7b008…` stay unchanged. Ownership is unchanged: the
existing `cover` owns sorted-range coalescing, packed-byte relocation and staged-byte preservation;
the map owns geometry, count, codec, admission and publication. The right33-entry destination is
completely overwritten before any read, so it needs no fill; the left32-entry initialized prefix
fills only uncovered gaps from canonical memory, preserving earlier staged bytes. Fully admitted
canonical source pages are backed; every initialized unbacked entry of a newly minted or
recycled-then-ACTIVE page must already be staged, so gap fills never read unbacked bytes. No API,
layout, fixture, observer, benchmark or FreeSpan change; no second editor, cache or allocator.

**Reservation rationale (peak, not final equality):** the successful path already reserves12 range
descriptors and `1040 + 32 + 64 + directory_bytes` bytes; the two pre-covers plus the remaining
metadata writes need at most **9 new range descriptors** and **`1040 + 32 + 42 + directory_bytes`
bytes**, including interacting earlier ranges, which only decrease newly needed coverage. This is
derived from the span set and the monotone byte union — not from spare capacity or final equality.

**Native facts (unchanged-source finals):** split reads **92→61**, read bytes **1352 unchanged**,
max read **32→512**, writes **8**, write bytes **1146**, max write **560**; the scratch/dirty record
(1146 dirty,8 ranges, byte cap1168, range cap12, reserves2) and the other four complete records are
unchanged. The left fill is one **512-byte** canonical fill (first-key validation staged no8bytes).
Cover totals **C74→D76**, **general73→10** and **contained1→66** are **source accounting**, not new
runtime counts and not IC component prices; the contained path still performs its search.

**Measured gates (binding; every original gate FAILS):**

| Scope | D | C | original ceiling | outcome |
| --- | ---: | ---: | ---: | --- |
| `scalar_replace_64` | 151560 | 151560 | 77576 | FAIL +73984 |
| `scalar_missing_64` | 136872 | 136872 | 85672 | FAIL +51200 |
| `scalar_tail_pairs_64` | 562954 | 562954 | 298171 | FAIL +264783 |
| `scalar_split_directory` | 70627 | 150572 | 30930 | FAIL +39697; **−79945 vs C (−53.094200781%)** |
| scalar aggregate (reference only) | 926946 | 1006891 | 497282 | reference, **not a seventh gate** |
| LARA total | 2185529 | 2185529 | 1527576 | FAIL +657953 |
| LARA named | 2184568 | 2184568 | 1526615 | FAIL +657953 |

Split's reduction is a **measured candidate-level benefit in this fixture**; the other three scalar
phases and both LARA rows are exactly equal to C (LARA delta0, wrapper gap961 unchanged). There is
no component attribution, generalized cost prediction, noise allowance or waiver. C's **+32 versus
reader150540** remains a historical failed directional prediction — not D's direction. CSV
persisted-YAML delta columns are different comparisons. Zero page growth is not zero allocation.

**Evidence (retained local roots; local artifacts are not a complete durable export or seal):**
initial D `$TMPDIR/spill-D-native-7410251a`; left-fill `$TMPDIR/spill-D-left-fill-e18ab287`; scalar
`$TMPDIR/spill-D-scalar-1cb0d853`; LARA `$TMPDIR/spill-D-lara-d421ea32`; five inherited sensitivities
`$TMPDIR/spill-D-five-8ef154dc-{count,resource,early,disjoint,backed}`; six final controls
`$TMPDIR/spill-D-final-86de9452-{format,map_clippy,lara_clippy,map,free,doc}`. Durable copies:
[scalar CSV](sources/adr0097-map-precover-D-scalar.csv) ·
[LARA CSV](sources/adr0097-map-precover-D-allocator.csv) ·
[source delta](sources/adr0097-map-precover-D-source.patch) ·
[final refusal](sources/adr0097-map-precover-D-plan-final.txt) (expected exit1: exactly
`bounded-proof` pending and criteria5/6 — plan0340 is not completed).

Six sensitivities are **six invocations of four test functions**, not six independent functions
(count+resource share `prepared_edits_resources_and_retry`; disjoint+backed share
`edit_image_read_partition`). The left-fill mutant dies during the97-entry seeding's pos64 split at
the initial exact pair assertion (**32 zero pairs, still97 rows**), before any timed pos1 split or
`SCALAR_WORK` record — not a historical C defect and not a standalone timed-split measurement. The
five older faults each reached their named oracle with `101/primary0/cleanup0` and exact D
restoration. Final **18/27/5** includes **one** D five-doctest run (C's two remain historical): four
compile-fails (E0499/E0502/E0061/E0382) plus one runtime example; lint scopes are map
lib+tests+canbench and separate LARA lib-only, with warnings retained outside those scopes. Artifact
identities: scalar Wasm **705490** bytes `40fd03438b5fd03abe41837b1bae732da07d364a8efe2aadb490cea88d93d274`;
LARA Wasm **5036159** bytes `e94f3140c4d9116355de7f8f5daf08c1ec7e59d27eedad998759c7f70501fce9`;
final map native **2215472** `9339d01eda4bc0accaba492bea07babc34ff3afda27f037a3e7339caef15964b`
and FreeSpan native **39490368** `376aff5a30db30c1600547ee9283fafb161359e7b7896108e1ef00b31b80576a`
(paths in each root's `binary.json`). The final map native hash differs from the initial D
`92be1a1a…`; the cause is unestablished, and source identity is not a codegen-equivalence claim.
Source, version, config and actual-used-lock bindings are
retained per root; no later live-target backfill. Integer receipt samples and fresh slices do not
establish old600/120 aggregate compliance. No new IC measurement or binary analysis was performed
in this documentation slice.

## Frozen-C execution checkpoint

**Historical checkpoint — no longer current status; at its time: status Experimental, functional checks complete, performance BLOCKED.**
Frozen C = map lib `a9e1581d2b8630766ff8d39129f37c449adaa971b3345be6b43c9db2e8ae2855` (the
shared-reader `5da46e45…` candidate plus the one guarded page-count reuse in `EditPlan::page`) with
benchmark `2f95a494f6c35e38f4480841210b7768c3c11d0afe7866bd3fa932150fc158e7`, FreeSpan
`35b7b00899db94ef58f6f6777ae2b29f9e6bc4a3c46a5dd4762e7067f87349fd` and control bench
`d4f2de85ca4cda643fc77a14168fa38ad198d1a22c6cf10fb028100dc54ea592` unchanged. The one source
delta is conservative snapshot reuse: while the dirty-range set is empty, `EditPlan::page` reuses
the `initial_pages` snapshot instead of re-reading the count; any staged write retains the
prospective count reading (prospective count writes pass through `image.write`/`cover` and leave the
dirty set nonempty; `cover` owns dirty-range storage and coalescing, not the page-count domain
rule). No new persistent cache,
owner, API or layout was added and backing validation is not weakened. The exact delta is
[adr0097-map-count-C-source.patch](sources/adr0097-map-count-C-source.patch).

Functional evidence (accepted): initial and final map18 each with all33 distinct `IMAGE_READ_CASE`
lines and the five complete `SCALAR_WORK` traffic/scratch records; all **five** intended runtime
sensitivities reached their exact oracles with contemporaneously archived executables (count, early,
resource, disjoint, backed); two independent five-doctest passes (E0499/E0502/E0061/E0382 plus the
runtime example); FreeSpan27; final formatting; map `--lib --tests --features canbench` clippy and a
separate LARA `--lib` clippy — the canbench-feature bench warnings and LARA lib-test warnings remain
observed outside that lint scope, not waived.

Performance (binding; every named gate fails):

| Scope | Frozen C | Saved reader5da46 | Original ceiling |
| --- | ---: | ---: | ---: |
| `scalar_replace_64` | 151560 | 153288 | 77576 |
| `scalar_missing_64` | 136872 | 138600 | 85672 |
| `scalar_tail_pairs_64` | 562954 | 566346 | 298171 |
| `scalar_split_directory` | 150572 (**+32 vs reader; direction falsified**) | 150540 | 30930 |
| scalar aggregate (comparison only) | 1006891 | 1013707 | 497282 |
| LARA total | 2185529 | 2191979 | 1527576 |
| LARA named | 2184568 | 2191018 | 1526615 |

All six original IC phase/control ceilings fail; the aggregate is comparison information, not an
averaging waiver. No component prices, noise tolerance, global zero-allocation or IC transaction
proof follows (zero heap/stable page growth is not zero allocation). These were separate bounded
slices, not a completed historical600/120 aggregate schedule; plan0340 is not completed
(`bounded-proof` pending, criteria5/6 unchecked).

Evidence: [index](sources/adr0097-map-count-C-checkpoint.json) ·
[scalar CSV](sources/adr0097-map-count-C-scalar.csv) ·
[LARA CSV](sources/adr0097-map-count-C-allocator.csv) ·
[source delta](sources/adr0097-map-count-C-source.patch) ·
[final refusal](sources/adr0097-map-count-C-plan-final.txt) (exit1: exactly `bounded-proof` pending
and criteria5/6 unchecked).

**Prior docs-closure protocol verdict (current record): INCOMPLETE/non-compliant.** Authorization
`79190daf` was followed by provider error `743adbbb` (`Connection error.`), then tool calls beginning
`da31b9e4` ran the prior setup/edits/finish with no intervening user reauthorization; that
authorization stopped at the first provider failure, and whether the harness or the agent initiated
the retry is not established. Matching literals and valid resulting files do not make that
continuation compliant; the error is not relabeled as harmless and the old receipt is not repaired.
This documentary violation is distinct from the **15 earlier separately reviewed validation
invocations** indexed in [checkpoint.json](sources/adr0097-map-count-C-checkpoint.json), which are
neither invalidated nor rerun; that index remains an immutable pre-correction documentary snapshot
(its recorded document hashes `12ead8b6…`/`424fecb2…`/`0e727264…` identify the pre-correction text),
not a seal of the current three documents. The correction receipt is
[adr0097-map-count-C-doc-review.txt](sources/adr0097-map-count-C-doc-review.txt).

## Shared-reader execution checkpoint

**Historical shared-reader checkpoint (superseded as current status by the frozen-C checkpoint
above); diagnostic scope only, no performance waiver.** The exact
[authorization](sources/adr0097-map-read-attempt-authorization.json) followed review of plan
`ecb7968ebbc66536dc8ea212d10c0990cb7dc1f6a287cc059318474a9e9fc17c`, preserved in
[the reviewed snapshot](sources/adr0097-map-read-diagnosis-plan.txt) and the new attempt's `before/`.
Only `crates/ic-stable-paged-ordered-map/src/lib.rs` changed in Rust. The
[contract](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#proposed-fully-backed-disjoint-reads)
remains owned by `EditImage::read`: reuse its partition/candidate, require the whole nonwrapping
interval within actual backing and disjoint from pending ranges, then read a nonempty destination
once and return. Contained and prefix/zero/overlay handling, callers, arithmetic/error order,
admission, codec, containers, editor/split/cover and publication remain byte-identical.

Source bindings:

- Before: `9436291caf0d7c2e878839eecf986ac7246236ba3410d8a2dc1fa08ab43e6b71`.
- Oracle-only snapshot, written before production: `95ff6ee6ec4898a52e30b231674dc318c67d2c20af101cc4d37fc07841d2f4a8`.
- Frozen candidate/final: `5da46e455eb5a321e3e2c7f4670c259e72f6a1d6c3d1f883554e1a4780ad456a`.
- Benchmark: `2f95a494f6c35e38f4480841210b7768c3c11d0afe7866bd3fa932150fc158e7`;
  FreeSpan: `35b7b00899db94ef58f6f6777ae2b29f9e6bc4a3c46a5dd4762e7067f87349fd` (both unchanged).

The [source audit](sources/adr0097-map-read-attempt-source-audit.json),
[production patch](sources/adr0097-map-read-attempt-production.patch) and
[complete scoped patch](sources/adr0097-map-read-attempt-source.patch) bind the sole branch and
appended test. All original17 tests/helpers, five doctests, observer, scalar oracle and seeder remain
unchanged. No signature/helper/type/module/dependency/state/API change, scalar mode/cache, second
search, inlining attribute or unsafe storage was added. No baseline was restored or rebuilt.

### Reader oracle and four reached sensitivities

`edit_image_read_partition` adds33 named cases over one `WorkMemory`, three actual pages and five
private images. The test asserts real inline storage (two ranges/24bytes) and real heap storage
(four ranges/48bytes); includes both disjoint adjacencies/gaps, packed-offset containment,
partial/multiple overlaps, zero length and staged/unstaged unbacked padding; and poisons each output.
Independent explicit bytes are checked **before** exact traffic, then no writes/grows and unchanged
backing. Setup/snapshot work is excluded from traffic. It adds no map, Graph or PocketIC constructor.
At `B=196608`, page63's owner-derived start is `B`, and its planned end is `B+1056`, not actual backing.
The required `[B-4,B+8)` oracle is `21 22 a1 a2 a3 a4 00 00 b1 b2 00 00`; unstaged straddles and
fully staged unbacked cases are separate. Invalid old-page admission is still caller-owned and unchanged.

The [oracle-first record](sources/adr0097-map-read-attempt-oracle-first.json) proves insertion order,
not an old-source runtime red: this new helper test was not run against9436 and makes no new public
reachability claim. Initial and final map runs each passed18 tests and reached all33 named cases.
The existing actual-grow3968 witness remains a public preparation/admission test, not a substitute
for this helper matrix.

| Separate mutation | Actual first runtime failure | Restoration |
| --- | --- | --- |
| Early publication after edit0 | `late DirectoryFull changed backing` | exact5da46e45 |
| Deferred backing admission | `expected actual restricted grow3/+2 before ready` | exact5da46e45 |
| Omit only disjointness | `IMAGE_READ inline-contained bytes`: actual `[19,20,21,22]`, expected `[162,163,164,165]` | exact5da46e45 |
| Omit only `end <= backed_end` | `unbacked-unstaged`, `B+16`/8bytes: `vec_mem.rs:36:13`, `read: out of bounds` | exact5da46e45 |

Each run compiled and reached one failing test (0passed/1failed, exit101). The full-backing mutation
reaches a real backing OOB panic, **not** a later assertion; its later byte/traffic assertions and
subsequent cases were not reached. The resource mutation's deferred publisher and later assertions
were also not reached. See the distinct [early](sources/adr0097-map-read-attempt-mutation-early.txt),
[resource](sources/adr0097-map-read-attempt-mutation-resource.txt),
[disjointness](sources/adr0097-map-read-attempt-mutation-disjoint.txt) and
[backing](sources/adr0097-map-read-attempt-mutation-backed.txt) receipts and their source patches.
There was no intended-failure retry or second candidate.

### Native, IC and emitted-route results

Final [map18](sources/adr0097-map-read-attempt-final-map.txt),
[FreeSpan27](sources/adr0097-map-read-attempt-final-free.txt) and
[doc5](sources/adr0097-map-read-attempt-final-doc.txt) passed, as did formatting and scoped strict
clippy (map `--lib --tests`, LARA `--lib`). The four compile-fail examples retain E0502, E0499, E0061
and E0382. No full-suite, LARA test-target lint or PocketIC result is claimed.
[Native observations](sources/adr0097-map-read-attempt-final-native.json) are identical initially,
finally and against9436: replacement9reads/112B,1write/8B; missing12/128,0/0; tail insert12/128,3/26;
tail removal13/136,2/10; split94/1368,8/1146. Basics retain at most32B reads and zero observed owner
dynamic reserves/capacities/growth; missing has zero dirty bytes/ranges. Split's scratch is unchanged.
These counters do not measure all process/backend allocation, and baseline scratch remains unobserved.

Exactly one unchanged scalar benchmark and one unchanged-source LARA control ran. Every original
independent gate fails; scalar total is diagnostic, not another independent gate or an average waiver.
All scopes retain their call counts, with zero heap/stable-page increases. Detailed comparisons in
[measurements](sources/adr0097-map-read-attempt-measurements.json) use retained references, not a
fresh simultaneous A/B campaign; raw [scalar](sources/adr0097-map-read-attempt-scalar.csv) and
[LARA](sources/adr0097-map-read-attempt-allocator.csv) CSV percentages against persisted YAML are not
admission-reference deltas.

| Scope | Admission ceiling | Blocked be81 | Corrected9436 | Reader5da4 | Versus admission | Versus9436 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Replacement64 | 77576 | 251720 | 167624 | 153288 | +97.597195% | −8.552475% |
| Missing64 | 85672 | 242856 | 158312 | 138600 | +61.779811% | −12.451362% |
| Tail64pairs | 298171 | 776778 | 607562 | 566346 | +89.940001% | −6.783834% |
| Split/directory | 30930 | 155692 | 155448 | 150540 | +386.711930% | −3.157326% |
| Scalar total (diagnostic) | 497282 | 1431979 | 1093879 | 1013707 | +103.849526% | −7.329147% |
| LARA total | 1527576 | 2350463 | 2256530 | 2191979 | +43.493941% | −2.860631% |
| LARA named | 1526615 | 2349502 | 2255569 | 2191018 | +43.521320% | −2.861850% |

The predictions **missing<158312** and **replacement<167624** hold, but were never acceptance
ceilings. The one scalar Wasm is698686B, SHA
`8297480840fa90929b855f4e72629aa9012069938703813606d87e573298cd8f`; LARA is5026122B,
`9b1a625a1b5402181f7d905cdf347ee5c9345bae8c04bede65a7ca478c3a2068`.
Both were archived before output-path reuse. Only the scalar received new disassembly, with pinned
`/opt/homebrew/Cellar/binaryen/132/bin/wasm-dis`132, executable hash
`644465f704a4d27e5bfae0d8914e83df73dc83e0516d1827086e57113f2f7bb5`.

The [static record and numbered excerpts](sources/adr0097-map-read-attempt-wasm.txt) show the whole
reader at WAT422–1295. Bounds at601 onward select the fully-backed route; absent/disjoint candidate
branches out of `block10` to full-length `stable64_read` at760 and then `block15`/return, skipping
the generic clamp, prefix read at827, tail-fill at870 and overlay tail. This is a source-bound
emitted route, not WAT-length pricing. The complete search, scalar and split caller excerpts are
byte-identical to9436. Scalar adjustment therefore remains304B; no peak-stack or Wasm object-layout
claim follows. Its caller initialization/admission/result/publication work and split normalizer remain.

Zero-length native cases make no `Memory` read, as the Rust guard requires. This Ic0 monomorphization
has no separate zero-length branch on the direct route: all35 named call references are direct
positive constants (24×8B,7×16B,4×32B), with no export/`ref.func`/dynamic reference to this helper.
No artificial zero-input Wasm probe or optimizer-cause claim is made. The measurements price only
the compiled guard's joint effect, not individual reads/checks, all remaining reader overhead or
split's residual. Improving instructions with unchanged native traffic does not waive any ceiling.

### Reader closure

[Lizard](sources/adr0097-map-read-attempt-lizard.json) on the exact9436/current production projection:
97functions unchanged; CCN314→318, NLOC1249→1256, tokens9840→9891, max37 unchanged. The sole touched
`EditImage::read` changes CCN7→11, NLOC31→38, tokens317→368; four parser-counted parameters include
`self` and are unchanged. The appended test separately measures CCN5/NLOC271/tokens2000/zero
parameters. No suppression, extraction, new interface or production abstraction was introduced;
its data table is not production complexity or runtime pricing.

The fresh [ledger](sources/adr0097-map-read-attempt-ledger.json) closes incomplete at
**222.56230762507766/600s**, including **50.341530542005785/120s** final use. The fixed320s pre-final
and120s final caps were respected;160s was unassigned and unused. There were15 scheduled
Rust/format/IC invocations, four intended runtime negatives, one scalar disassembly and no unexpected
command failure/timeout. Driver patterns were bound to the observed rustfmt single-line guard
before any mutation invocation; the initial driver is preserved, and no Rust revision/retry followed.

Actual-used Cargo/canbench lock `f5d323d0…`, CSVs and emitted executables were archived per invocation;
checkout lock `15d08391…`, absent map CSV and exact prior LARA CSV were restored in finally before
subsequent invocations. Initial native map `cbde62ce…` and final `5ede72df…` are separately bound to
5da4, not asserted identical. Missing historical native hashes remain missing. See
[run provenance](sources/adr0097-map-read-attempt-run.json),
[scope/protected/history verification](sources/adr0097-map-read-attempt-verification.json) and
[source-bound plan refusal](sources/adr0097-map-read-attempt-plan-check.txt).

The same plan retains three completed TODOs and four checked criteria; `bounded-proof` and
criteria5/6 remain pending/unchecked. The final structural validator refuses exactly those items,
not a falsely green plan. Raw root: `$TMPDIR/spill-map-read-9436-j1wy264j/`; this attempt's manifest,
source snapshots, commands, failures, artifacts and durables are separate from all13 protected
historical manifests/1021 entries. HEAD/staging, foreign inputs, completed0339, prior ceilings and
the original main-reference failure remain unchanged. Shared main was not queried or modified.
No unused balance, reference rebuild, second patch, broader benchmark, full A0/A1 implementation or
landing is authorized. The next step is review of this bounded diagnostic result, not tuning.

## Next performance decision — one fully specified diagnostic measurement (revised, not authorized)

**Revision after review515d9868; four premises withdrawn, prior decision superseded.** The
withdrawal record is in
[revision provenance](sources/adr0097-map-read-decision-revision.json); the corrected premises
and the complete executable plan are in
[analysis v2](sources/adr0097-map-read-decision-analysis-v2.json), with the first-draft receipt
preserved byte-unchanged. Withdrawn: (1) the guard experiment's split −4,908 (and the other
phase deltas) is the joint effect of ONE guard — not an upper bound on remaining
reader/canonical-routing cost — and fewer native reads do not bound IC instructions; (2)
≥8,448 modeled copy bytes cannot imply ≤~5% of a516,425-instruction residual — a byte lower
bound is neither a cost upper bound nor a measured elimination benefit, and
codegen/control/normalization remain unpriced; (3) `scalar_edit` creates a fresh `EditImage` on
every public call (lib.rs1328–1334) while the fixture runs separate scalar calls (bench.rs
blocks149/156/162/169), so no cross-call dirty-slot staging story stands — any source trace
must live within one call's image, canonical bytes are distinct from staged bytes, and neither
replace−missing=22,784/≈356 (different lookup routes) nor missing+52,928 (one no-write
operation's excess) is a write-cost bound or cross-phase floor; (4) required
validation/arithmetic/error ordering stay fixed, but present zero-init/container/call
implementations are not shown mandatory — the sufficient, truthful claim is only that
existing evidence cannot rank components.

**Selected next step: ONE cfg-gated diagnostic measurement, fully specified in v2.** Mechanism
chosen from pinned canbench-rs0.7.0 (sha recorded): static accumulator counters over
`ic_cdk::api::performance_counter(0)` — the map's `canbench` feature already enables
ic-cdk0.20.1, and canbench-rs's own counter/reset are private while its BenchScope path
(global per-name Vec/BTreeMap aggregation, Drop bookkeeping charged to the enclosing
interval, cross-name nesting double-count) cannot isolate phase×category. Exact owner surface:
marks at `scalar_edit` (M0–M4 init/prepare/result/publish siblings with M1–M3
shared endpoints), `EditImage::read`, `EditImage::cover` (all callers) and `admit_image` —
the v2 read-wrap/write-excl sub-intervals are dropped as unsupported by the calibration
contract; per-phase isolation by
snapshot+reset between the four existing phase blocks after seeding/sanity; emission after
final validation via `ic_cdk::eprintln!` + `--show-canister-output`. Calibration is
observation-only — three matched shapes (K=64 standalone leaf, K=16 one-level-nested, K=32
shared-endpoint) emit min/mean/max under defined timestamps/reset/accumulator/anti-elision;
these are empirical observations, explicitly NOT bounds: raw intervals are reported with no
certified E-bounded ranking ('cannot rank' is an accepted outcome), the saved-row Δ check
survives only as an empirical reject-only sanity check (cancellation can pass it; no
attribution proof), and `prepare_rest`/loop-drop remainders stay definitional. Fail-closed =
exact equality against the WEIGHTED per-operation-kind count table pre-registered from frozen
source (tail cover =64×3+64×2=320 — insert≠remove; homogeneous quotients forbidden; interval
pairs, function visits and physical endpoints kept distinct with M1–M3 shared; each
read/cover/admit visit finalizes once at every early-return site with errors/control flow
unchanged) — full table in the
[protocol revision](sources/adr0097-map-read-decision-protocol-revision.json). The cfg-on
semantic check is the fixture's
own pre/post asserts plus `validate()` inside the same instrumented wasm; the source audit
requires every added hunk cfg-gated and cfg-stripped source byte-equal to `5da46e45…`.
LARA exclusion is **configuration-scoped**: in the scheduled feature set map
`canbench = ["dep:canbench-rs", "dep:candid", "dep:ic-cdk"]` while LARA
`canbench = ["dep:canbench-rs", "dep:candid"]` does not feature-enable the map dependency and
no LARA canbench build is scheduled — Cargo feature unification or explicit forwarding could
enable instrumentation in another build, so no absolute claim; no LARA run, no feature
forwarding. Instrumented component costs may rank current-source
candidates only; they cannot attribute per-component regressions versus uninstrumented
admission and cannot reconstruct unperturbed cost. Fresh ledger proposal in the
[protocol revision](sources/adr0097-map-read-decision-protocol-revision.json): maximum600 =
ordered pre-final caps215 — now including ALL FOUR mutation oracles
(early/resource/disjoint-guard/backing-guard, each restored and asserted at sha(M)) — plus
protected final120 and **explicitly unassigned265**, with the source-at-each-step contract,
per-invocation native+Wasm/lock/CSV capture, stop rules and cfg-off-preservation vs
cfg-on-coverage separation fixed there; no old balance. No execution is authorized; acceptance remains
BLOCKED with every original ceiling failing.

**Execution endpoint and protocol/evidence refusal (review739d119d).** The schedule was
subsequently executed under review dfaed91c, but its protocol outcome is
**INCOMPLETE/non-compliant**: after the first authorized stop (setup `FileNotFoundError`) and a
second gate failure (map archive/finally tail), the runner was repaired and reactivated twice —
the justification "none of the enumerated stop categories occurred" is withdrawn, and neither
restoration nor spare charged capacity authorized continuation; no later allowance
retroactively validates the overrun. The real observations are retained: initial and final
**18** map tests (**33** oracle cases), **5** doctests (×2), **27** FreeSpan tests, four
runtime-red mutations with exact messages and M-restoration, cfg-on fixture assertions
passing inside the instrumented wasm (canbench exit0), `clippy --features canbench` exit0,
and an exact count/calibration oracle (endpoints1600/1856/4480/343; read512/704/1472/94;
cover64/0/320/74; siblings64/64/128/1; calibration64/16/32/1, delta4542). Present endpoint:
formatted **M = lib `2c76e95637493bda72af9913cf237f5a0089bc9723731d440eb193f0e676cf89` /
bench `363d1d65e70c94c673db1d71d415ad9f009a3e5713a014ce0192c9cd527c6378`** (frozen originals
`5da46e45…`/`2f95a494…` preserved; M_draft receipts stay byte-preserved). Evidence gaps are
part of the verdict: the sealed112-entry manifest does not bind the final `ledger.json`
(`35247b4b…` recorded vs `e42fcc94…` actual — external coverage issued, old manifest/seal
untouched); the initial map executable has no contemporaneous archive (later overwritten;
only final-map's `663e4a81…` is contemporaneous for its own invocation; no rebuild offered);
the saved-row "sanity gate" was never implemented — observation-only, no post-hoc cutoff; and
`components.csv` has a3-column header vs4-column rows (flawed original preserved; rectangular
re-export issued separately). Definitional remainders from the saved sums — read/cover/
prepare_rest: replace154624/35648/246592, missing211136/0/277952, tail441280/246592/735616,
split33706/128248/65451; loop/call/drop/recorder:26626/25251/49697/893 — are arithmetic on
instrumented intervals, not independent validation; with the200-instruction standalone
calibration (min=max) and saved-row deltas up to +1,951,474, no calibration subtraction and
**no uninstrumented component ranking** is claimed. Recorded charges remain exactly
**135.69941875094082**; correction/resealing work has no recorded timing and is not asserted
cap-compliant. Resolution verdict, chronology, charges, coverage and rectangular table:
[reconciliation](sources/adr0097-map-read-reconcile-reconciliation.json),
[coverage](sources/adr0097-map-read-reconcile-coverage.json),
[table](sources/adr0097-map-read-reconcile-components.json). `bounded-proof` pending;
criteria5/6 unchecked; acceptance BLOCKED.

**Tooling preflight and retained-observation status (review904f04bc; historical pending-C endpoint —
its remaining-validation wording predates the executed five-sensitivity and frozen-C slices).** g1 remains
permanently stopped/incomplete (~23.293s recorded charge; this turn's desk-check/py_compile
work lies outside its measured entries, so no whole-turn cap-compliance is claimed); its
930-line `run_guard.py` is retired and byte-preserved. Reviewer-confirmed retained
observations from the saved streams: map stdout reports **18 passed /0 failed**, stderr holds
all **33** image-read cases, and the five `SCALAR_WORK` rows match the preregistered native
predictions **8/104,11/120,11/120,12/128,92/1352** with every other numeric field equal to
saved `5da46` `final-native.json` — despite the driver's skipped reach check. They do **not**
fill the missing contemporaneous initial-map executable archive (a later live-target copy is
not retroactive), prove the five sensitivities, or supply any IC/final-validation evidence.
Frozen endpoint unchanged: **C = lib
`a9e1581d2b8630766ff8d39129f37c449adaa971b3345be6b43c9db2e8ae2855` / bench `2f95a494…`** with
the single guarded count expression. A separate20s tooling-only preflight exercised the
mechanical fragments (fail-closed artifact capture with recorded-cwd authority and
restoration-under-capture-raise on temp-only lock/CSV sentinels; exact whole-block in-memory
count-mutant with byte-identical inverse; qualified single-row CSV lookup with duplicate/
missing errors and authoring-fixed ceiling keys; saved stdout/stderr native matcher) — **PASS
in0.0244s**, receipt `$TMPDIR/spill-tooling-preflight-f297-t1/receipt.json` (sha
`8761e77e0720cab70088ef91091518a5cf8c50d94924b5e32dfda7cacc6d9c0b`). Remaining validation for
frozen C awaits separate review and a fresh budget, with source mutation limited to the five
previously approved temporary sensitivities and exact-C restoration; g1's ledger and its
unused155s remain closed. Plan criteria5/6 remain unresolved; engine/A0/A1/ADR0097/landing
stay blocked.

## Remaining prospective read path

**Historical source/artifact diagnosis; the correction was then unimplemented.** Inputs are corrected source
`9436291c…`, its saved scalar WAT `14ca515f…`/Wasm `5b4264f1…`, and blocked WAT `5e7d28a8…`/Wasm
`37842bd4…`. All hashes were checked against the closed artifacts. No tool was invoked to rebuild,
execute or redisassemble either binary. The separate
[inspection record](sources/adr0097-map-read-diagnosis-inspection.json) and
[numbered excerpts](sources/adr0097-map-read-diagnosis-wasm.txt) preserve exact paths, symbols and
full-file line references. The corrected and blocked `EditImage::read` functions have identical
text after substituting only three panic-path `slice_index_fail` location operands1063128→1063144.
Their successful-path code is unchanged. The previous joint patch did not simplify this helper;
that observation does not assign it the remaining IC regression.

### Common path, with evidence limits

`EditPlan::read<const N>` remains the sole source caller of `EditImage::read`: initialize `[0; N]`,
borrow it as a slice, call the helper, then return/decode. The saved Wasm materializes zeroed
8/16/32-byte destinations and an out-of-line image-read call. The key-search loop likewise zeros
its8-byte slot, calls the helper, loads and compares the key, and advances or stops. The reader's
835 WAT lines include mutually exclusive branches and panic paths; their size is not dynamic work
or an instruction-price measurement. It has no own linear-memory stack adjustment in this body;
the search function has16B and scalar304B adjustments, neither a peak-stack measurement.

For the recorded97-entry fixture, the source-derived read routing is:

| Work before any scalar dirty write | Missing129 | Replace64 |
| --- | ---: | ---: |
| Initial canonical page-count read outside the image helper | 1 ×8B | 1 ×8B |
| Prospective directory length | 1 ×8B | 1 ×8B |
| Directory entries | 1 ×16B (index1) | 2 ×16B (indices1,0) |
| Prospective page count / selected header / first key | 1 ×8B /1 ×32B /1 ×8B | same |
| Key probes | 6 ×8B:192,160,144,136,132,128 | 1 ×8B:64 |
| Previous value | none | 1 ×8B |
| Image-helper calls on that route | 11 | 8 |

All those image reads see an empty image and fully backed addresses. Every scalar invocation has
its own image; prior timed replacements have already published their value to canonical backing.
The totals agree with retained native12/128 and9/112 reads, but11/8 are **source-derived routes**,
not new Wasm call counters or per-read IC prices. The test-only staged-value observer adds one
fully covered image read after replacement; it adds no canonical read and is absent from timed
canbench code. No new native/IC measurement was run in this pass.

The saved helper's empty-image route still selects inline/heap ranges, validates the inline slice
length, dispatches the empty partition and computes the request end. Its zero-length partition
executes **no binary-search iteration**; the absent candidate skips full-containment handling.
It then computes saturating backed length/minimum, checks the destination prefix, performs the
stable read, checks the tail slice and tests its length. Fully backed input makes that tail empty:
**no `memory.fill` executes**. The `first == ranges.len()` branch exits before overlay setup/copy.
There is no payload overlay on this route, but the general bounded-prefix/tail decision work remains.

For a nonempty image, partitioning skips ranges ending at/before the request. If all are left of it,
the route rejoins that empty-tail exit. If the next range starts at/after the request end, current
code still performs the bounded canonical read, tail checks and byte-container selection before the
first overlay iteration breaks. Neither case copies pending bytes. A range covering the whole
request instead returns staged bytes immediately, without any canonical read. Partial overlap
reads the backed prefix, zeros only the unavailable suffix, then copies all intersecting pending
pieces; the source distinguishes both ends and gaps between multiple ranges.

The surrounding work remains present but unpriced. `EditPlan::new` reads backing size and initial
page count with checked page-byte multiplication; prior-result capacity is reserved before ordered
intents. Lookup still admits directory length, checked selected-page offset/full-page end and
allocated/link bounds, then ACTIVE/length/self-link/free-link/minimum before key search. Checked
address multiplication still emits `__multi3`. Missing removal stores inline `None`; replacement
reads the prior value and stages8 bytes through unchanged reserve/write/cover. Both check final
backing admission (no grow here), extract the prior result, visit publication ranges and run container
drop discriminant checks. Missing has no ranges/writes; replacement has one8-byte write. Inline
basics do not take the owner heap-deallocation branches. None of that work disappears merely because
the former image transfers are gone. The proposed change leaves it and the caller zeroing intact.

### One selected correction and what would falsify it

Ranked leads, not a multi-candidate campaign:

1. **General read routing on canonical requests.** If this contributes, skipping only that routing
   lowers missing/replacement with the same canonical I/O; this is the selected bounded experiment.
2. **Call/initialization and retained preparation/admission/result work.** If these dominate, an
   emitted reader bypass can exist with little or no total-phase recovery. Those costs remain
   controls; neither removing required validation nor altering call boundaries is proposed.
3. **Structural range normalization.** If repeated split staging dominates its residual, common-read
   savings can coexist with a persistently expensive split. No normalizer experiment is proposed.

The strongest locally supported small target is redundant general read routing on proven canonical
requests—not all common overhead. Propose only the
[fully-backed disjoint case at `EditImage::read`](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#proposed-fully-backed-disjoint-reads).
After its existing partition, a nonwrapping interval entirely inside actual backing, with no pending
intersection, can read the whole destination and return. It must not use the requested first byte
alone, empty ranges alone, or `required_end` as proof. Its empty and nonempty forms share that one
owner-level rule; no scalar-specific reader, cached mode or second helper is selected.

This skips the clamp/tail/overlay-tail path only when its result is already established. Full and
partial staged reads, including staged bytes beyond backing, keep the existing implementation.
New-page gaps still zero-pad without out-of-bounds reads; invalid old-page geometry still rejects
at its unchanged caller check. Public ownership, full allocated backing before ready and error
order remain controls. A larger helper split/const-generic or inlining experiment might affect
call/initialization costs, but its benefit is not established and it is deliberately excluded.

The falsifiable prediction is unchanged native I/O/scratch with **missing64 below158312 and
replacement64 below167624** on one frozen candidate. A matched future WAT must actually route these
eligible reads to a full stable read/return without the generic clamp/tail path. If the compiler
retains that work or either phase does not improve, this correction is insufficient. Even if it
improves both, the result prices only that compiled guard's joint effect, not all reader overhead or
the previous image/codec patch. Added eligibility checks can cost overlapping reads; the unchanged
LARA and structural scopes remain independent gates. No guaranteed saving or recovery is asserted.

**Coverage gap and bounded acquisition.** Current scalar and batch tests remain necessary: exact
priors/repeated-key changes, partial header/directory updates, split/reuse/drain, selected corrupt
page2 only after edit0 staging, and the real grow at3968. They reject many backing-only shortcuts.
However, their results do not prove the private reader's complete disjoint/partial/unbacked padding
matrix; new-page fields used during the existing growth batch can already be fully staged. The
proposed additional `edit_image_read_partition` unit uses one real3-page `WorkMemory`, independent
byte expectations, poisoned destinations and both actual inline/heap storage. It explicitly covers
full/partial/multiple overlaps, nonzero packed source offsets, adjacency/gaps, zero-length reads and
partly/wholly unbacked new-page intervals. It constructs no map/Graph/PocketIC. Its byte-level
straddle is a helper seam, not a newly observed public aligned request. The ADR owns exact cases and
expected traffic. No existing assertion/helper/fixture changes.

Two new mutations would separately remove disjointness or full-backing eligibility. The first must
fail staged-byte expectations; the second must reach the real backing bounds panic on the labelled
unbacked case, not be credited with a later unreached assertion. Both original mutations remain.
The [same0340 proposal](../../plans/0340-prepared-paged-map-edits.md#proposed-read-case-validation--not-authorized)
requests a fresh600s maximum/120s final reserve (320s pre-final caps;160s unassigned), one corrected
scalar run, one unchanged LARA control, proposed18/27/5 checks and those four mutations. This is not
use of the closed230.668119/60.072337 ledger or its unassigned180s. No unchanged baseline run,
second candidate, tuning loop or execution is authorized now. The missing evidence is the proposed
guard's emitted route/runtime effect and the added oracles' runtime sensitivity—not another broad
survey or a speculative attribution from native read counts.

**Structural residual stays separate.** Split still repeats lookup/admission and stages65 entries
through `set_entry → write → cover`; directory suffix covering is already implemented. The saved
split code retains per-entry normalization, including left-side prepending of staged payload. That
is a distinct static lead, not a measured copy count or proof of whole-batch quadratic complexity.
The former joint patch changed split155692→155448, only244 instructions. The new guard might affect
some of its reads but cannot be assumed to explain its remaining+402.580% or authorize cover/split
changes. Replacement+116.077%, missing+84.7885%, tail+103.763%, LARA total+47.7197%/named+47.7497%
and every original ceiling remain binding. Source-only diagnosis is complete; implementation and
performance acceptance remain blocked. Separate [plan snapshot](sources/adr0097-map-read-diagnosis-plan.txt),
[structural refusals](sources/adr0097-map-read-diagnosis-plan-check.txt) and
[provenance](sources/adr0097-map-read-diagnosis-provenance.json) leave all executed histories intact.

## Caller-owned image correction checkpoint

**Verdict: still blocked; one authorized correction, no second candidate.** Only
`crates/ic-stable-paged-ordered-map/src/lib.rs` changed in Rust, from `be81cd30…` to
`9436291caf0d7c2e878839eecf986ac7246236ba3410d8a2dc1fa08ab43e6b71`. Each caller initializes its image;
`prepare_image` fills `&mut EditImage` and returns `Result<(), FailedEdit>`, `EditPlan` borrows it,
and private publication reads `&EditImage`. Public `PreparedEdits::publish/discard` still consume the
exclusive owner-bound capability. No self-reference, new type, mode flag or scalar-specific editor.

`PageHeader::decode/encode` owns the former `page`, `set_page`, canonical reader and test-only writer
mappings. The now-unused `read_u16`, `write_u8`, `write_u16` helpers are deleted after caller audit.
Canonical header traffic deliberately changes **5 reads/27B→1/32**; prospective reads stay **1/32**.
Admission is not moved into decoding. All selected-page/link/free checks, caller/error ordering,
result reservation, full minted-page backing and responsible grow index3968 remain. Basic writes
still touch only changed fields; encoding retains zero padding.

The [source audit](sources/adr0097-map-correction-source-audit.json) and
[correction diff](sources/adr0097-map-correction-source.patch) preserve this boundary. All17 test
bodies, five doctest examples, seeder, oracle, counting Memory/observer, `Scratch`, overlay read/write/
cover, `apply_edit`, split loops and directory algorithms are byte-identical. Only the existing direct
split adapter's image lifetime changed mechanically. Benchmark `2f95a494…`, FreeSpan `35b7b008…`,
accepted guards, controls and foreign artifacts stayed frozen. The source was frozen before IC;
no tuning occurred between measurement, dependency control and final checks.

### Re-established contracts and static result

Final validation passed17/0 map tests,27/0 frozen FreeSpan tests,5/0 doctests, touched formatting and
strict two-library clippy. The compile-fail diagnostics remain E0502/E0499/E0061/E0382. Both mutations
reached unchanged runtime oracles (0/1, exit101): `late DirectoryFull changed backing` and
`expected actual restricted grow3/+2 before ready`. Both restored exactly to `9436291c…`. As before,
the second mutation rejects premature readiness; its deferred publisher and later assertions were
not reached. Tests retained normal scheduling; no process-global failpoint or thread override.

Initial and final native observations are identical to each other and to the blocked candidate's
recorded traffic/scratch fields: replacement9/112 reads and1/8 writes; missing12/128 and0/0;
tail insertion12/128 and3/26; tail removal13/136 and2/10. Basic reads are at most32B, with zero
owner-dynamic capacities/reservations/grows. Missing removal retains zero dirty bytes/ranges.
Split remains94/1368 reads,8/1146 writes,1146 dirty bytes/eight ranges,1168 byte capacity/twelve range
slots and two reservations. Baseline scratch remains **unobserved**, not measured zero. Owner
counters and reported zero page growth do not measure every process/backend allocation.

One static disassembly used the newly archived scalar Wasm
`5b4264f12eeb271d679f55616b60f7b305190b7249749220ea2fc1fd79a74dc8` (698629B), with the same hash-matched
Homebrew Binaryen132 tool. LARA Wasm
`17ac422d558198568b148a47ca59dd17246d45a0ac953816638d68f3b867d4c6` (5025931B) was archived, not
disassembled. [Static excerpts](sources/adr0097-map-correction-wasm.txt) bind full WAT
`14ca515f6091ef144ca9ea0d7f8811478cc4dad06800a41f0482a775b77167e6`. The complete emitted scalar function
has **no direct `memory.copy` or `memory.fill` instruction**: the previously identified image-transfer
sequence is absent, while initialization stores remain. Its static stack-pointer adjustment is
**752→304B**. Out-of-line image reads, checked arithmetic, storage dispatch, backing/result handling,
empty publication traversal and drop work remain; split still calls the per-entry normalizer.
304B is one function's adjustment, not peak call-chain stack or Wasm image layout. Native192B is a
separate fact. No baseline binary was rebuilt or retrospective baseline hash manufactured.

### Every original IC gate remains unmet

The comparison uses retained `ae76d90f…` (approved-admission production plus identical measurement
additions) and `be81cd30…`, not a newly concurrent baseline campaign. Source/tool/config/fixture and
actual-used-lock comparability were checked. Both [scalar CSV](sources/adr0097-map-correction-scalar.csv)
and [LARA CSV](sources/adr0097-map-correction-allocator.csv) are exact generated bytes; the
[comparison](sources/adr0097-map-correction-measurements.json) records both reference deltas.

| Scope | Admission ceiling | Blocked candidate | Correction | vs admission | vs blocked |
| --- | ---: | ---: | ---: | ---: | ---: |
| Replacement64 | 77,576 | 251,720 | 167,624 | +90,048 /+116.0771% | −84,096 /−33.4085% |
| Missing64 | 85,672 | 242,856 | 158,312 | +72,640 /+84.7885% | −84,544 /−34.8124% |
| Tail64 pairs | 298,171 | 776,778 | 607,562 | +309,391 /+103.7629% | −169,216 /−21.7843% |
| Split/directory | 30,930 | 155,692 | 155,448 | +124,518 /+402.5800% | −244 /−0.1567% |
| Scalar total | 497,282 | 1,431,979 | 1,093,879 | +596,597 /+119.9716% | −338,100 /−23.6107% |
| LARA total | 1,527,576 | 2,350,463 | 2,256,530 | +728,954 /+47.7197% | −93,933 /−3.9964% |
| LARA named scope | 1,526,615 | 2,349,502 | 2,255,569 | +728,954 /+47.7497% | −93,933 /−3.9980% |

All phases/control report zero heap/stable-page growth. LARA's raw CSV +737,248/+48.53% is relative
to persisted YAML, **not** the retained admission comparison above. No averaging, noise allowance,
aggregate waiver or necessary-cost explanation is established. Fewer image transfers plus missing
158312<242856 supports a contribution from the selected correction. It does **not** isolate the IC
price of copies from codec/compiler effects, recover the original gate, or explain the residual
split cost. The split improvement of244 instructions does not authorize a normalizer optimization.

### Correction closure

[Lizard1.24.0](sources/adr0097-map-correction-lizard.json), explicit single-file mode against frozen
`be81cd30…`, reports owner functions96→97, CCN313→314, NLOC1226→1249, tokens9804→9840 and unchanged
maximum37. Directly touched functions11→11, CCN30→30, maximum9→9, NLOC105→123 and tokens1066→1056.
The small source increase centralizes header mapping at its existing owner and changes private
borrowing; no public API, container or editor was added. The source projection excludes unchanged
test nodes/seeder and is not compiler analysis. No metric gaming or speed claim follows.

Raw evidence is `$TMPDIR/spill-map-correction-be81-1611h8iy/`. The
[run record](sources/adr0097-map-correction-run.json) and [new ledger](sources/adr0097-map-correction-ledger.json)
close at230.668119/600s, with60.072337/120s final use. Twelve Rust/format/IC invocations completed;
only the two intended mutations returned101. No unexpected compile/command failure or timeout.
Setup/source/collection and both final administrative entries are charged; the180s unassigned
balance was unavailable. No prior balance was spent, and unused time is now closed.

Each invocation archived its actual-used lock and emitted native test/Wasm artifact before reuse
of an output path, restored checkout lock `15d08391…` and prior per-crate CSV bytes/presence in a
finally path, then allowed the next invocation. Cargo/canbench used retained lock `f5d323d0…`.
The first corrected native executable is `6803d178…`; the final native executable is `33b050db…`.
Both are separately archived and source-bound, not asserted byte-identical. The historical baseline
native executable hash remains missing. No baseline restoration/build, cold target, dependency
installation, YAML write, old-runner reuse, cleanup, full suite, PocketIC, commit or merge occurred.

[Plan snapshot](sources/adr0097-map-correction-plan.txt) and
[final refusal](sources/adr0097-map-correction-plan-check.txt) record three completed TODOs and one
pending `bounded-proof`, with criteria1–4 checked and5/6 unchecked. `shared-images` is complete;
performance and overall completion are not. Completed0339, all eleven prior manifests, old receipts,
foreign artifacts, original reference-gate failure and eleven inherited unavailable links remain
unchanged. HEAD is still f297; shared main was neither queried nor modified. Full A0 acquisition/
capacity/alias authority, A1 whole-operation publication and later native/Graph/reclamation work
remain separately gated. Review this frozen result before authorizing anything further.

## Map owner execution checkpoint

**Historical checkpoint: this section and the diagnosis below refer to `be81cd30…`, not the later
correction.** Their original evidence and failed gates remain unchanged.

**Verdict: blocked, not accepted.** The authorized two-file candidate adds `Edit`, indexed
`PrepareError`, and non-Clone, owner-bound `PreparedEdits`. Scalar and batch edits use
`EditPlan::apply_edit`; `publish_image` only writes prepared ranges. No layout/dependency change,
LARA writer edit, custom cleanup Drop, external ready-image accessor or second scalar editor was
introduced. Initialization remains separate. `reserve_for_inserts` retains both nonwriting guards;
only its obsolete allocator comment changed. Scalar `OutOfMemory` wording now includes scratch.

- Final map source: `be81cd30103fda176a1d6517f12fc2f2b28f90960c11d7243bc8d0a40f209460`.
- Identical A/B benchmark file: `2f95a494f6c35e38f4480841210b7768c3c11d0afe7866bd3fa932150fc158e7`;
  shared97-entry seeder is byte-identical. Baseline measurement-only map source was `ae76d90f…`.
- FreeSpan remains `35b7b008…`; approved admission map `41873db3…` is the comparison snapshot,
  not the new candidate's approval. HEAD remains `f297706ab…`.
- Raw execution: `$TMPDIR/spill-map-f297-cecc5rw_/`. The
  [run record](sources/adr0097-map-run.json), [source audit](sources/adr0097-map-source-verification.json),
  [measurements](sources/adr0097-map-measurements.json) and [ledger](sources/adr0097-map-ledger.json)
  are separate from every prior checkpoint.

### Functional and resource evidence

The baseline-compatible oracle ran with the existing twelve tests **before production changes**:
13/0. Final source passed17/0 map tests,27/0 unchanged FreeSpan tests,5/0 doctests, touched formatting
and strict **library** clippy. Normal test scheduling remained enabled. The four compile failures
reached E0502 (mutation), E0499 (second preparation), E0061 (redirected publication), and E0382
(second consume), not setup/import failures. The existing successful doctest still ran.

Ordered/discard coverage checks exact priors, minima, page reuse **before** drain can hide it,
drain/reset/reinsert and independent final pairs. The valid8192-page/8255-entry directory fixture
rejects update128→insert192 at edit1, including with trailing remove256; remove256 first permits
exact page2 reuse. Only its existing setup was extracted; all twelve accepted assertions/contracts
were compared against the baseline, along with the unchanged reservation executable body.

The corruption witness changes only page2's ACTIVE flags byte to FREE. The final observer reads the
actual staged replacement bytes, not the requested input: edit0's prior/new value is recorded before
`(edit1,page2,FREE)` rejects with exact `Edit { at: 1, source: Corrupt }`. Poisoned backing remains
unchanged; byte restoration permits exact retry and scalar/batch parity. The arithmetic overflow
probe is owner-local, not public huge-state reachability evidence.

The restricted-memory fixture genuinely needs growth3/+2 for126 data pages after4000 intents,
returning `Grow` at3968, the intent establishing the final allocated extent. A separately labelled
byte-scratch fault at0 is cleared alone; the identical batch then reaches the unchanged backing
restriction. Reopening that same backing with the larger range permits growth, explicit discard,
then exact retry/publish of4064 pairs. This proves owner-level rejection, not IC rollback.

Both sensitivities failed at runtime (0/1, exit101): early publication changed backing before the
late DirectoryFull; deferring the **real** backing admission returned ready where grow3/+2 had to
reject. The latter stops at that strong assertion, so its deferred publisher and later scratch
checks were not reached. Both mutations were restored to their exact source snapshot. Subsequent
source-audited refinements preserved those assertions: equivalent split arithmetic, actual-byte
observer, layout output, isolated scratch retry and corrected comments/error wording. All final
checks and both candidate IC runs bind the resulting `be81cd30…` source.

| Single native operation | Reads calls/bytes, before → final | Writes calls/bytes, before → final |
| --- | ---: | ---: |
| Replace64 | 17/123 → 9/112 | 2/16 → 1/8 |
| Missing remove129 | 22/163 → 12/128 | 0/0 → 0/0 |
| Tail insert2000 | 32/217 → 12/128 | 8/51 → 3/26 |
| Tail remove2000 | 29/206 → 13/136 | 6/35 → 2/10 |
| Actual split/directory shift | 186/1423 → 94/1368 | 160/1221 → 8/1146 |

All four basic phases meet their exact write gates, ≤32B read transfer, and zero owner-dynamic
byte/range/result capacity or reserve attempts. Final size calls are one per operation; baseline
size calls were2/0/8/6 (split160). No native grow occurred in these scalar phases. Baseline scratch
is **unobserved**, with source audit, not runtime zero. The split retains1146 dirty bytes/eight ranges,
byte capacity1168 and twelve range slots, with two reserve attempts. It uses one dirty-byte buffer
for the1040 payload bytes across left/right ranges, not a second split vector; directory shifts
materialize and move the affected suffix once. This small shift does not price8192 shifted entries.

The4000-intent image has134554 dirty bytes/seven ranges, byte capacity147456, range capacity24 and
result capacity4000. Native `size_of` is192B for the image,24B per range,16B per result; inline
container objects are48/88/32B. These are not compiler frame sizes or Wasm sizes. Owner counters
exclude arbitrary backend/process allocations; snapshots do not prove absence of transient writes.

### IC gate failure and remaining owner review

| Exact paired scope | Before | Candidate | Delta | Change |
| --- | ---: | ---: | ---: | ---: |
| Replacement64 | 77,576 | 251,720 | +174,144 | +224.4818% |
| Missing remove64 | 85,672 | 242,856 | +157,184 | +183.4718% |
| Tail insert/remove64 pairs | 298,171 | 776,778 | +478,607 | +160.5143% |
| One split/directory shift | 30,930 | 155,692 | +124,762 | +403.3689% |
| Scalar benchmark total | 497,282 | 1,431,979 | +934,697 | +187.9612% |
| LARA split/restore total | 1,527,576 | 2,350,463 | +822,887 | +53.8688% |

The LARA named scope is1,526,615→2,349,502 (+822,887/+53.9027%). Its raw CSV's +831,181 comparison
is against persisted YAML, **not** this pair. All four invocations reported zero heap/stable-page
growth, which is neither zero allocation nor a regression waiver. Neither smaller native traffic
nor summing/averaging scopes excuses these increases. The combined map change is priced; individual
check, image movement, decoding and buffer-bookkeeping instruction costs are not isolated. No
necessary-cost or noise explanation is established. No optimization retry or extra campaign ran.

Lizard1.24.0 compares the approved admission snapshot with the final source, removing test-only
nodes in a preserved line-aligned projection. Owner functions70→96, CCN189→313, NLOC767→1226,
tokens5696→9804; directly touched functions25→56, CCN81→210, maximum12→37. The new benchmark is
one function, CCN6/NLOC68. Material owners include `apply_edit` (37/113 CCN/NLOC), `split_page`
(23/89), and `cover` (12/79); old split was12/67. These are source metrics, not speed evidence;
raw parameter counts can include tuple commas/receivers. See the [full metrics](sources/adr0097-map-lizard.json).

The single editor and consuming capability are useful boundaries, but the implementation's cost
and complexity are **not accepted**. A concrete source-review item also remains: prospective
`EditPlan::page`/`set_page` spell page-header slice offsets separately from the canonical
`read_page_header` field mapping. Normalize that at the existing header owner before treating the
shared-image work as complete; do not add another decoder or tune a second scalar path. The candidate
is left frozen for review, not silently patched after its final-source IC evidence.

### Execution closure and exclusions

The new ledger charges237.227102/600s, including44.351155/120s in the final reserve. Fifteen Rust/
format/IC commands completed; two expected runtime negatives are not compiler failures. One
administrative collector failed because normal-scheduling `ok` output was glued to `max_write`.
The shared parser was corrected after inspection; raw log, initial script/failure and correction
remain preserved, without a Rust rerun. The final completion validator refuses the still-pending
shared-image/performance criteria; no final approval is claimed.

All four actual CSVs and as-used lock/build hashes are retained. Each invocation restored its
crate's prior CSV bytes/presence and original checkout lock before the next. Wasm hashes are in the
[build record](sources/adr0097-map-native-build.json); final native binary hashing is retained, but
**baseline native binary hashing was not captured**. Source/lock/compile/runtime transcripts remain;
there was no retrospective rerun. The frozen historical manifests, plan0339, prose receipts,
foreign brief/config/YAML/CSV, and original failed main-reference gate remain unchanged. Inherited
unavailable links remain disclosed debt, not newly repaired or silently ignored history.

The final [plan snapshot](sources/adr0097-map-plan.txt) and
[completion refusal](sources/adr0097-map-plan-check.txt) preserve the incomplete state. Unused time
is closed, not authority for more Rust work. No full LARA suite, wider map campaign, PocketIC,
allocator/capacity authority, mixed-origin A0, A1 publication, native deletion/demotion, reclamation,
Graph integration, commit, merge or landing approval follows.

### Source and Wasm diagnosis

**Source/prose-only pass; candidate remains frozen.** The review request permits diagnosis and
revision of future work in0340, not implementation or use of either closed ledger. The source
comparison is approved-admission map `41873db3…` versus blocked map `be81cd30…`; the measurement-only
baseline `ae76d90f…` adds the identical oracle/seeder. It is not HEAD-before-admission or an
unrecorded baseline restoration. Benchmark `2f95a494…` and FreeSpan `35b7b008…` remain unchanged.
All earlier CSVs, failure receipts, snapshots and approvals keep their original source binding.

**Missing remove129 first.** Old `remove` calls `find_page_for_key`, then `page_search`, returning
`Ok(None)` immediately on the failed search. For this33-entry page2, directory lookup reads the
selected directory entry twice; page search reads the header's five fields and six key/value pairs,
including six unused values. The recorded22reads/163B and zero writes agree with that source path.
It does not establish the stronger candidate admission checks.

Current `remove → scalar_edit → prepare_image` constructs an `EditImage` and an owning `EditPlan`,
reads backing size/initial page count, reserves the inline prior-result slot, and calls the shared
`apply_edit`. That checks directory length, locates page2, checks its address/full old-page backing
and allocated/link bounds, then checks ACTIVE, length, self-links, free link and directory minimum
before six key probes establish absence. The recorded12reads/128B comprise initial page count,
directory length, one directory entry, prospective page count, one32B header, first key and six
search keys. These checks are not payload copying and cannot be dropped to regain the old cost.

There is still common preparation work after `Ok(None)`: store the inline result, check backing
admission (no grow), return the image by value, extract the result, pass the image by value into
`publish_image`, visit its empty range slice and drop three inline-or-heap containers. At source
level every prospective read goes through `EditImage::read`'s range selection, backing clamp and
possible overlay handling, even while the image has no ranges. There are no dirty-range extensions,
payload writes or actual heap reservations on this operation. Thus copying dirty bytes/publication
writes alone cannot explain the measured+157,184 instructions across64 missing removals.
`Result`/enum matches preserve typed errors and storage variants; their mere presence is not a
measured cost attribution or grounds to erase error distinctions.

**Replacement and tails.** They share that entry/exit/read flow. Old replacement reads the prior
entry and rewrites both key and value via two `safe_write` calls; current replacement reserves
8B/one range, writes only the value into the image, then publishes once. Old tail insertion/removal
rewrite five header fields and map length; current insertion stages16B+2B+8B and removal2B+8B.
Their entry-shift loops execute zero iterations here. Current `reserve_scratch` performs capacity
checks even when all storage stays inline; `write → cover` establishes/coalesces the1/3/2 ranges.
That is additional machinery, but the missing operation shows it is not the entire common cause.
The recorded9/112,12/128,13/136 reads and exact8/26/10 written bytes do not establish lower
instructions. The original native ceilings and replacement/tail-pair IC gates remain binding.

**Actual split1.** Old `split_page_and_insert` rejects DirectoryFull, finds directory membership,
reads64 old pairs into a65-entry `Vec`, uses `split_off` for the right half, allocates a page and
publishes per-field/per-entry writes, links, directory and length. Those vector allocations are
source facts, not baseline scratch measurements. Current `apply_edit` first performs selected-page
admission/search; `split_page` then rereads directory length, searches membership, repeats page
admission/key search and checks length before scratch reservation and prospective allocation. That direct split seam also
serves the existing admission test; its error order must not be casually hoisted or bypassed.

The current split emits33 right entries forward and32 left entries backwards through
`set_entry → image.write → cover`, preserving sources dirtied by earlier intents. Each extension
normalizes sorted ranges and packed bytes again. In this fixture, prepending left entries can move
already-staged right payload; some other `copy_within` calls can be empty or same-address. This is a
concrete structural cost lead, not a measured copy-byte count or proof of quadratic whole-batch
behavior. The65 payload emissions are not65 stable writes: the final image is1146B/eight writes.
The existing directory helper already covers the affected suffix once and uses `copy_within`;
do not propose that already-implemented change as the fix. No second payload vector is proposed.

**Existing build inspection.** Both named target Wasm files still match the recorded candidate
hashes: scalar `37842bd4…` (700310B), LARA `7d32ecc9…` (5025086B). The baseline hashes `0f4741b8…`
and `8c3cd394…` remain recorded, but neither baseline binary is at those output paths and the closed
execution manifest archives no Wasm. This is a scoped availability statement, not an exhaustive
filesystem search or a claim that a recorded hash is an available artifact. Baseline native binary
hashing is still absent; no retrospective reconstruction occurred.

Only the matched scalar Wasm was disassembled, using the existing Homebrew Binaryen132
`wasm-dis`; LARA was hash-checked, not disassembled. The initially found PATH `wasm-dis --version`
failed with empty output; its exact child exit was not saved and its cause is unknown. File/hash
inspection identified the distinct installed Homebrew binary. No installation, repair, retry of
the failed binary, Rust command or Wasm execution occurred. The local successful static commands
and the first failure are retained in the [diagnostic provenance](sources/adr0097-map-diagnosis-provenance.json).

The [bounded WAT excerpts](sources/adr0097-map-diagnosis-wasm.txt) show:

- `scalar_edit` subtracts752 from the linear-memory stack pointer at entry. This is a static
  adjustment for one compiled function, not total call-chain stack, peak usage, native frame size
  or a measurement of `EditImage`'s Wasm layout. The native192B object remains a different fact.
- Before its first directory-read call, it initializes116B with `memory.fill` and transfers104B
  twice. The successful tail contains two further104B transfers and one144B transfer before the
  range/write loop. The valid missing-remove path joins that success tail despite having no dirty
  ranges. These are observed compiled operations, not inferred elimination of Rust moves or an IC
  price assigned to each byte; stores and other bookkeeping also remain.
- `EditImage::read` remains an out-of-line helper with inline/heap range selection, empty-range
  dispatch, backing clamp and stable read; zero ranges skip overlay copying, not the whole helper.
  Checked page arithmetic includes `__multi3` and bounds checks. Split's two loops call the compiled
  `EditImage::write` normalizer per entry. Static code establishes presence, not the fraction of the
  measured regression attributable to calls, arithmetic, transfers or normalization.

**Ranked hypotheses and chosen diagnostic delta.** (1) Avoidable image-value transfers contribute
to common overhead: supported by compiled transfers, but their instruction share is unknown.
(2) General overlay/container bookkeeping on empty or small images contributes across searches:
compiled presence is established, marginal IC cost is not. (3) Added selected-page/backing/checked
arithmetic admission contributes: required checks are preserved, not priced or waived. (4) Per-entry
range normalization and repeated lookup amplify actual split cost: source/Wasm support that path,
not its measured share. The missing-remove phase distinguishes common work from split-only work.

Select only the [ADR-owned caller-owned-image/codec correction](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#proposed-correction-within-the-map-owner).
It deletes intermediate owned image handoffs and four header mappings while retaining the shared
editor, guards, error order, containers, overlay algorithm and split normalizer. Do not bundle a
pre-cover rewrite, second scalar editor or blanket inlining. Canonical header reads intentionally
change5calls/27B→1call/32B; prospective header reads stay1/32. This side effect must be priced by the
unchanged dependency control, not hidden as a free normalization. The patch is a bounded hypothesis,
not a promise to recover all four phases or a reason to accept residual regression.

The single minimal performance experiment, **only after fresh implementation/execution authority**,
is one run of unchanged `canbench bench_paged_scalar_edits_97 --csv` on that correction. Compare each
scope both with the retained blocked candidate and the immutable approved-admission measurements;
do not rebuild either unchanged baseline. Inspect the newly hash-bound scalar Wasm once to see
whether the identified image transfers disappeared or shrank. Fewer transfers plus lower missing
cost than242856 supports this correction's contribution; fewer transfers with unchanged/higher
missing cost shows it is insufficient, not permission for another tuning loop. Lower cost alone
does not isolate transfers from codec/compiler effects. Unqualified acceptance still requires each
phase at or below its original baseline, not merely better than the blocked candidate.

The same0340 plan proposes, but does not authorize, a fresh serialized600s maximum with120s protected
for final checks, the unchanged17/27/5 contracts, both existing sensitivities and one unchanged LARA
control. No new fixture size, benchmark series or baseline campaign. All incomplete criteria remain
unchecked; checked items remain historical evidence, not certification of the proposed correction.
Separate [revised plan snapshot](sources/adr0097-map-diagnosis-plan.txt) and
[structural refusals](sources/adr0097-map-diagnosis-plan-check.txt) preserve this distinction.
The earlier executed0340 snapshot/refusal, completed0339, ten historical manifests and original
failed reference gate remain untouched. Full A0/A1 and later owners remain outside this proposal.

## Reduced admission implementation

The source scope, approval and measurements in this section describe the preserved admission
checkpoint, not the later map candidate above.

Only `lara/edge/free_span.rs` and `ic-stable-paged-ordered-map/src/lib.rs` changed in Rust.
The [candidate patch](sources/adr0097-admission-candidate.patch),
[run metadata](sources/adr0097-admission-run.json) and
[ordered ledger](sources/adr0097-admission-ledger.json) identify the exact tested source:

- Free-span SHA256: `35b7b00899db94ef58f6f6777ae2b29f9e6bc4a3c46a5dd4762e7067f87349fd`.
- Paged-map SHA256: `41873db32cd7fb056646fcac3d7ee42d8f6ea96c9d0d8701d45479fd15d80f6d`.

**Invariant owner:** `exact_span_record` now resolves the index key and validates active flag,
stored start and exact length before destructive removal. It replaces the separate exactness
facade and duplicated checks in removal/replacement; remove-and-reinsert compensation is gone.
Pair admission retains left/right, replacement empty/end and neighbor-error precedence, then rejects
identical input IDs and an unrelated replacement-key collision before either removal. Each removal
fetches its record afresh, so the second removal never uses bin links cached before the first.
The fast `restore_allocated_prefix` body is byte-identical to the base and still uses its known-successor
path. This is not atomicity for a valid pair's subsequent index/bin/heap failure.

Map reservation returns immediately for zero. Positive requests retain saturating sizing and
`GrowFailed`; `need.div_ceil(WASM_PAGE_SIZE) <= memory.size()` avoids both an already-backed sentinel
write and overflow-prone `size * page_bytes` arithmetic. No public API, schema, layout, guard,
prepared capability, heap redesign or dependency was added. Directory admission remains separate.

### Reached witnesses and final validation

The [baseline witness patch](sources/adr0097-admission-witness.patch) changed only tests.
All four oracles survived implementation and both mutations; only formatting and optional trailing
commas changed between baseline and formatted source. All 25/10 legacy test bodies are byte-identical.
Tests used normal scheduling; commands were serialized, with no thread-count override.

| Witness | Baseline reached result | Repaired/final result |
| --- | --- | --- |
| Exact span | Exact `MissingSpan` and unchanged 65 logical spans, but public index roots changed **1/2 → 2/1** after remove/reinsert; full backing/cache equality failed. The actual singleton + full-page geometry was reached. | Roots remain 1/2; complete record/index backing, heap and summaries unchanged. No map wire-layout logic was copied into LARA. |
| Pair | Earlier precedence checks passed; identical-input `MissingSpan` then failed backing/heap equality. **The second replacement-key case was preempted**, not an old-source runtime red. | Both identical-input and unrelated-key cases pass exact errors, all three owned spans and unchanged state, with preceding-error controls. |
| Reservation | 64 injectively valued public inserts reached the final byte; zero reservation changed the last value **`0xa50000000000003f` → `63`**, failing complete backing equality. Positive repeated reservation was preempted. | Zero and repeated already-backed positive reservation preserve exact bytes/values/header/size; a nonzero unreferenced sentinel distinguishes skipping a write from rewriting zero. |
| Directory | Fully backed/validated **8255 entries / 8192 pages**; reserve-one succeeds, insert-192 returns exact `DirectoryFull` unchanged. Remove-256/retry-192 reuses page 2 and preserves exact ordered values, links/counts and split ownership. **Passed on old source.** | Same already-safe control passes; it is not a batch-atomicity or public-insertion-reachability claim. |

Baseline filters: [free-span](sources/adr0097-admission-before-free.txt) **0 passed / 2 failed**;
[map](sources/adr0097-admission-before-map.txt) **1 passed / 1 failed**. Green filters passed **2/0 + 2/0**.
Restoring the original removal ordering failed the unchanged exact-span oracle (**0/1**);
restoring the old reservation write failed the unchanged value/backing oracle (**0/1**).
[Both mutation patches/logs](sources/adr0097-admission-run.json) are retained and neither is in final source.
No compile/setup failure, timeout or Rust retry occurred.

Final [free-span filter](sources/adr0097-admission-final-free.txt): **27/0**, 5.401s command,
4.01s compile / 0.20s runtime. Final [map filter](sources/adr0097-admission-final-map.txt): **12/0**,
1.459s command, 1.00s compile / 0.08s runtime. Touched-file formatting passed;
[strict library clippy](sources/adr0097-admission-final-clippy.txt) passed in 2.415s.
The 15 pre-existing LARA test warnings were not suppressed. This is not a full LARA/workspace suite.
The final [plan structural check](sources/adr0097-admission-final-plan.txt) passed (exit 0, 0.068s),
bound to plan SHA256 `b64ce26a3da069ced124533926d503689f6855ea7298e47fcadcede7199efae1`.
Structural validity is separate from implementation or full-A0 approval.

**Fixture cost:** each complete added-test pass uses 3 free-span stores + 2 standalone maps, no
new memory specialization. Across baseline/green/mutations/final: **9 added-test free-span stores +
7 standalone maps**; the baseline pair constructed only its first store. Legacy final filters add
25 fresh free-span fixtures, 6 successful reopens and 3 expected rejected initialization attempts;
11 fresh standalone maps + 2 reopens. Allocator-owned indexes are not counted as separate map
fixtures. The four IC runs each construct one allocator benchmark fixture. Zero LARA graphs,
PocketIC constructors or canister installs. Full backing/cache snapshots are test allocations,
not instrumentation proving absence of transient production writes or heap allocation.

### Paired IC controls and code review

Baseline controls ran before source/test edits; final controls ran after exact mutation/lock
restoration. Both benchmark bodies, setup/constants and configuration remain byte-identical.
[Measurements](sources/adr0097-admission-measurements.json) retain total and inclusive scope
separately; the table uses actual paired CSV instructions, not YAML-relative percentages.

| Unchanged total | Before | After | Paired delta |
| --- | ---: | ---: | ---: |
| `bench_r_fs_tkbfs_1024` | 1,519,282 | 1,527,576 | +8,294 / **+0.545916%** |
| `bench_r_fs_lbin_4096` | 5,084,163 | 5,171,235 | +87,072 / **+1.712612%** |

All four runs completed (55.723s, 3.561s, 50.084s, 3.318s); each reported zero heap/stable page
growth, not zero allocation. Whole-span removal now performs an index lookup before destructive
removal; the largest-bin control reaches that path. This identifies additional source work, not a
per-read IC attribution. The split/restore delta is not isolated from code-generation effects.
Independent review accepted these increases as this repair's scoped trade-off. They are not noise,
not an isolated per-read price or unavoidable-cost proof, and not a waiver for A0/A1.
No speculative optimization, new benchmark, drain re-enable, survey, 16K replay or persistence ran.
There is no archived before-Wasm binary or whole-batch/publication pricing claim.

[Lizard 1.24.0](sources/adr0097-admission-lizard.json), working tree against the base, measured only
the two files. Five touched production functions remain five: CCN **30 → 31**, NLOC **83 → 86**,
tokens **630 → 654**, maximum CCN **9 → 13**. Pair admission owns the added branches; the two
mutation helpers lose duplicated validation. Including the four tests and one snapshot helper,
touched functions are 5 → 10 and CCN 30 → 47. Existing complex untouched functions are excluded.
The change is focused: no alternate coalescer, cache, preparation framework or tuning abstraction.
Complexity figures do not establish performance.

### Closure and limitations

The closed ledger records **17 measured commands / 153.108726s + 80s administration = 233.108726s
of 600s**, leaving 366.891274s unused. Administration is 20s setup, 20s restoration and 40s final
evidence; expected reds remain charged. The protected 120s final reserve used 49.628039s.
Unused time grants no further execution authority.
Source/evidence checks reconstruct both mutations, compare unchanged oracles/legacy tests,
verify protected hashes, exact path scope and the immutable r2 manifest. Two administrative
collector failures are preserved: the default Python lacked Lizard, then whitespace-only comparison
rejected rustfmt's optional trailing commas. The installed Lizard interpreter and inspected
formatting diff corrected those checks without rerunning Rust. An untracked-evidence whitespace
check also found an extra EOF blank in a copied green log; durable log-only normalization records
both raw/durable hashes and preserves the original logs. Context-bearing raw patches are retained;
durable patch derivatives use zero-context hunks (`git apply --unidiff-zero`). Raw evidence remains
at `$TMPDIR/spill-admission-f297-6g1p_any/`; durable artifacts use `adr0097-admission-*`.

Each CSV was harvested and the foreign CSV restored and hash-checked before the next invocation,
including cleanup in the runner's `finally` path. As-used lock `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`
and restored lock `15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2` are retained;
the existing [as-used lock patch](sources/adr0097-crossover-as-used-lock.patch) describes those same
bytes. Canonical benchmark artifacts, foreign files, all original/r1/r2 snapshots and unrelated
source remained unchanged at admission closure. HEAD/main were unchanged then; nothing was staged,
committed or merged by that execution. The later shared-main drift is recorded separately below.

**Still open:** valid pair late-failure atomicity; complete A0 public checked edits/borrow authority,
interacting batches, resource/heap preparation and acquisition cancellation; A1 whole-unit peer/pin/
rewrite publication; reclamation policy, native deletion/demotion and complete/empty retirement;
Graph/queue/PocketIC/lifecycle and landing. Neither full backing equality nor a safe directory
control proves these boundaries. The saved deletion witness and assertion were untouched.

## Revision checklist

- **P1 — corrected; policy unresolved.** `S=1025,D=512` refutes the old claim that the target cap alone
  justifies never demoting a nonempty tree. A one-slot prefix plus a 1024-row existing run is a
  representation candidate, not implemented conversion. The ADR compares this with existing
  move-reporting explicit operations and cursor/observer maintenance. Their current tree skips,
  scan bounds, enqueue timing and caller capabilities remain gates; new persistent progress is
  not assumed necessary for every alternative. No retention/suppression branch is implemented.
- **P2 — corrected and resliced.** Per-vertex preparation is insufficient. The ADR defines all four
  outer first-write boundaries, whole-unit peer/pin/count/release preparation, disjoint versus
  overlap scratch, and one consuming publisher. A decisive late-peer witness is now required.
  Additional allocator source evidence makes A0 resource preparation a prerequisite to A1 wiring;
  returning a prepared object after nested relocation or wrapping the old release calls is rejected.
- **Empty retirement — gate retained and quantified.** Work follows historical `S`, not live `D`.
  Rooted-block and ID-vector bounds are analytical, not runtime or IC-budget evidence. Finite u32
  limits, native `Drop`, or an unchanged last-delete retry do not establish liveness or IC cleanup.

The user accepted r1's bounded P1/P2 corrections and A0/A1 separation **for further interface design
only**. That acceptance does not select reclamation policy, authorize implementation, or close
GAP001/003. Full-A0 interfaces remain unapproved. The separately authorized reduced admission
plan is now implemented and independently approved within its exact admission scope; that approval
does not complete the full-A0 obligations below.

## Recorded counterexample, not rerun

The [real public 513→512 witness](adr0097-tree-deletion-prerequisites.md#reachable-demotion-failure)
already supplies the red: after temporarily bypassing the stale width assertion, a valid 1024-slot,
33-byte tree committed the marker, degree and global decrement, then panicked comparing 33792
property bytes with 16896. The selected live edges and unfiltered property rows were unequal.
`catch_unwind` recorded the changed native state; it did not roll it back. All diagnostic changes
were restored. The assertion remains in source now. Successful-path silent packing and retirement
hazards are source-audited findings, not additional observed runtime failures.

GAP004's subsequent canonical admission repair fixes the older dead/missing public symptom; it
must not be reported as still broken or as evidence of live mutation safety. GAP006 fixes selected
hydration only. This design neither repeats the witness nor changes its oracle or accepted fixtures.

## Source ownership and callers

Paths below were inspected at the base plus the two hash-pinned admission changes. Names, not
historical plan-number comments, identify owners; local plan0339 is unrelated to older source
comments mentioning that number.

| Current owner / path | Actual behavior and planned boundary consequence |
| --- | --- |
| [`graph/remove.rs`](../../crates/ic-stable-lara/src/labeled/graph/remove.rs), `remove_edge_at_slot` → `remove_edge_at_slot_with_move` | Plain wrapper drops `EdgeRemoval.moves`. Tree dispatch deletes first, rereads the descriptor fallibly, then ignores `tree_mode_demote_to_slab`'s `Result`. It returns an empty move vector even though demotion packs. Replace this sequence only after preparation is safe; no-move callers are why deletion must preserve slots. |
| [`graph/tree_write.rs`](../../crates/ic-stable-lara/src/labeled/graph/tree_write.rs), `tree_mode_remove_edge_at_slot` | Width-zero assertion remains. Marker write precedes checked header increment, degree/global subtraction and descriptor publication. Partial compensation exists, not a preflight guarantee. Raw returned `E` is decoded from four topology bytes; native value hydration/slot-label adaptation for deletion observers must be prepared, not assumed. |
| Same file, `tree_mode_demote_to_slab` and `_pub` test accessor | Two separate edge/value collections, reservation of `degree` edge slots, two property-width-dependent publication/cleanup branches, then fallible accounting/resource discovery and ignored releases. The private test accessor must use the same surviving preparation/publication owner, not retain an alternative demoter. |
| Same file, `collect_leaf_block_ids`, edge/property native resolvers | Existing root/depth traversal concepts. Inventory must include all reachable interiors as well as leaves in both streams before publication. Width33 property fanout differs from topology fanout. Source metadata checks promised by GAP002 are not established by this audit. |
| [`graph/compact.rs`](../../crates/ic-stable-lara/src/labeled/graph/compact.rs), `bucket_resident_region`, `combined_span_region_len` | Physical root ownership is already centralized, distinct from logical high-water. Existing demotion recomputes an edge-only avoidance range instead; it must consume this owner rather than duplicate geometry. |
| Same file, `materialize_labeled_vertex_edge_plan`, `rewrite_vertex_edge_span_with_policy`, `commit_vertex_edge_span_layout` | Three vertex-commit call sites share raw physical publication but not an outer prepare boundary. Rewrite around lines 936–1023 may relocate and restart; relocation loops around 1356–1370 and 1381–1394 may publish an earlier peer before later failure. Disjoint relocation publishes the pin before the loops. Prepare the entire outer call, not each vertex independently. |
| [`graph/leaf_pin.rs`), placement/pin helpers; [`lara/edge/row_layout.rs`](../../crates/ic-stable-lara/src/lara/edge/row_layout.rs), count owner | Read current occupied ranges and distinguish existing pin, nonpublishing expansion reservation, new leaf cover and unpinned spans. The rewrite's final placement must be computed against a planned peer layout, using these existing geometry rules rather than first installing the new pin. |
| [`labeled/bucket_store.rs`](../../crates/ic-stable-lara/src/labeled/bucket_store.rs), `write_label_bucket_row_adaptive{,_with_scratch}` | Small rows write per slot; larger rows allocate/encode scratch during publication. Its wire/validation rule stays at this owner; physical preparation must not duplicate it or assume there is no fallible work in the callee. |
| [`graph/promote.rs`](../../crates/ic-stable-lara/src/labeled/graph/promote.rs), `transcribe_and_publish_tree` | Useful existing ownership precedent: unpublished allocations, retirement reservation and last recoverable reservation before publication. Not proof that every shared rewrite or demotion path is already atomic. |
| [`lara/edge/span.rs`](../../crates/ic-stable-lara/src/lara/edge/span.rs), `allocate_span_avoiding` / `release_span` | Takes from the free list before checking slot end; avoided candidates are returned through fallible release, otherwise tail allocation publishes capacity. Bare addresses carry no cancellation rights. Both facades use lossy zero/zero `GrowFailed` projections. The selected future path must not call them inside preparation. |
| [`lara/edge/edges.rs`](../../crates/ic-stable-lara/src/lara/edge/edges.rs), growth/header owner; [`lara/edge/init.rs`](../../crates/ic-stable-lara/src/lara/edge/init.rs), cached header | The 64-byte slab header and four-page growth reserve are owned here. `set_elem_capacity` publishes persisted capacity and then cached capacity; generic header writers can also republish it. Backing-only preparation must leave both copies unchanged and share authority with every capacity writer. |
| [`lara/edge/free_span.rs`](../../crates/ic-stable-lara/src/lara/edge/free_span.rs), `reserve_for_releases`, `release`, `record_max_candidate` | Reservation pre-grows record/index backing, but release still probes/coalesces intervals and can allocate/rebuild the lazy max heap. Whole-batch validation and bookkeeping must stay at this owner; stable-page reservation alone is not an infallible retirement proof. |
| [`ic-stable-paged-ordered-map/src/lib.rs`](../../crates/ic-stable-paged-ordered-map/src/lib.rs), `reserve_for_inserts`, `split_page_and_insert`, removal | Reservation covers page growth, not directory exhaustion or corruption. `split_page_and_insert` already rejects `DirectoryFull` and missing directory membership **before** writes; its 65-entry and second split vectors are separate resource obligations. `insert_into_page`, `remove_from_page` and `unlink_and_free_page` have later membership checks on different paths. `DIR_CAP=8192`, `PAGE_CAP=64`. A0 must establish the involved index edits' preconditions and scratch at the index owner, not copy directory rules into LARA. |
| [`lara/edge_inline_property.rs`](../../crates/ic-stable-lara/src/lara/edge_inline_property.rs), allocation/retirement helpers | Future B reuses independent byte-span allocation-origin restoration. A0/A1 physical copies do not allocate, rewrite or retire property payloads. Their approval is not proof of value allocation or native LTB retirement. |
| [`labeled/ltb_raw_block_store.rs`](../../crates/ic-stable-lara/src/labeled/ltb_raw_block_store.rs), `release` | Rejects unminted IDs, panics on double release, mutates the intrusive free list. Validate inventory before release, deduplicate by rejecting duplicate ownership, and retire once. Do not silently deduplicate a corrupt tree or suppress the failure. |
| [`bidirectional/deferred.rs`](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs), `remove_forward_edge_at_slot{,_with_move}`, `remove_reverse_edge_at_slot{,_with_move}` | Four public methods use canonical live admission, then the same single-orientation removal owner. They return orientation-wrapped errors; ordinary wrappers discard moves. Their post-success enqueue gate is bypass-only and returns early for these normal/tree rows: it is not a demotion retry queue. Both orientations need actual post-promotion live-delete coverage, not just the approved no-op matrix. |
| [`bidirectional/counterpart.rs`](../../crates/ic-stable-lara/src/labeled/bidirectional/counterpart.rs), `counterpart_of`, `counterpart_of_verified` | Equal-neighbor live rank determines the other occurrence. Parallel edges are not deduplicated by neighbor. Resolve the counterpart before deletion; `PairOrdinal` changes when an earlier occurrence disappears and must not be cached as identity. |
| [`Graph store/lookup.rs`](../../crates/graph/src/facade/store/lookup.rs), `with_graph_mut` | Only `GRAPH.with_borrow_mut(f)`; no mate-cache invalidation hook. Searches of the current delete/lookup/counterpart owners found no persistent mate cache on this execution path. This is narrower than saying the repository has no future mate-index designs. |
| [`store/adjacency.rs`](../../crates/graph/src/facade/store/adjacency.rs), `commit_delete_edge_by_handle` | Resolves canonical edge/counterpart and index-build plan first; fences/build-DML and clears sidecars before forward deletion. Applies returned moves, then removes the pre-resolved reverse occurrence (or other forward half for an undirected edge), then maintains. New LARA rejection after this fence must trap, not return a partially committed Graph error. |
| [`store/sidecar.rs`](../../crates/graph/src/facade/store/sidecar.rs), `apply_edge_slot_moves`, `relocate_edge_properties_for_move`; [`store/helpers.rs`](../../crates/graph/src/facade/store/helpers.rs), `GraphSidecarMoveObserver` | Synchronous deletion vectors and deferred maintenance notifications are distinct delivery paths. Forward moves relocate canonical sidecars and postings; reverse moves do not own canonical sidecars. Observer failure after canonical movement traps. No native rollback follows merely from that trap. |
| [`derived_state/reverse_adjacency.rs`](../../crates/graph/src/facade/derived_state/reverse_adjacency.rs), `reconcile_diverged_key` | Deletes pre-collected reverse slots in descending order through the no-move public wrapper. Packing during one deletion invalidates later requested coordinates. This concrete consumer rules out casually widening demotion's move list while leaving wrappers unchanged. |
| `graph/remove.rs`, `drain_out_edges_for_label`; deferred `process_delete_vertex_step` / `DeleteVertexV1` | Descending reserved positions, one delete step and a saved `removed_edges` continuation depend on stable coordinates. Existing `?` after queue pop can skip requeue on error; success-path in-flight preservation is not error-path proof. Matching-removal work inside the purge remains a separate GAP004 blocker. |
| [`store/maintenance.rs`](../../crates/graph/src/facade/store/maintenance.rs), deferred maintenance driver | Existing `CompactVertexEdgeSpanV1` carries bucket/slot continuation and forwards moves. Current producer/selector admission is slab-oriented and tree buckets advance without compaction. Compare extending that execution flow with explicit `compact_vertex_edge_span_with_moves` before requiring new persisted state; neither supports native reclamation today. GAP005's obsolete combined tag stays untouched. |

### First-write and fault classification audit

There are three different boundaries, not one transaction:

1. **Canonical LARA outer-operation boundary.** A rejected physical prepare preserves every
   participating row/root, peer, pin and count, not only the requested vertex. A destination write
   into a live source or early pin update is already canonical publication. Only disjoint owned
   target bytes/backing growth may precede it. The intended remainder has no recoverable failure.
   This begins at outer rewrite/relocation entry, not an earlier insertion or Graph mutation.
2. **Graph stable fence boundary.** Index-build admission, postings and sidecar deletion can occur
   before LARA writes. A LARA pre-write rejection can therefore be a Graph **post-fence** failure.
   Existing `trap_post_fence_commit` uses `panic!`; it must reach an uncaught IC entry-point trap,
   not a catch-and-continue/`Err` response. Native panic alone supplies no rollback.
3. **Queue/IC execution boundary.** Native maintenance may have popped work already. Restore the
   original key/continuation on a proven pre-canonical rejection; do not replay a half-committed pair
   as untouched. IC traps restore only the current message execution, not earlier delete ticks.

The official [canister-security skill](https://skills.internetcomputer.org/.well-known/skills/canister-security/SKILL.md)
was checked against the locked CDK 0.20.1/stable-structures 0.7.2. Its CDK example prerequisite is
older (0.19); no example code or cleanup/`Drop` claim is used as proof here. The
[IC interface specification](https://docs.internetcomputer.org/references/ic-interface-spec/canister-interface/#interpretation-of-numbers)
is the authority for discarding a trapping entry point's changes. The security guidance distinguishes
pre-call and response-callback executions. Native backing equality proves neither that rule nor
Graph's correct use of it. No cross-canister compensation or multi-message rollback is proposed.

## Stage A reslicing and interfaces

### Acquisition decision checkpoint

**Selected for review, not implementation:** nonpublishing destination acquisition and capacity
planning, combined with subsequent retirement in one owner-bound image. ADR0088 now owns the
[protocol, signatures, minimum fields, errors and resource bounds](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#planned-nonpublishing-acquisition-and-retirement).
The rejected alternative is a receipt requiring complete pre-reserved inverse publication, not just
a `(start,len)` return address. Neither option is present in Rust. The r2 public sketch was not adopted
merely because it existed: its release-only cancellation limit, blanket semantic-before-scratch order
and release-only maximum rule cannot be carried unchanged into mixed acquisition/retirement.

The normative trace plans one free-list and one grown-tail destination, seals **all** retirement,
fails final metadata, explicitly discards, and retries with identical starts. Physical backing may
remain grown; persisted/cached element capacity and canonical free ownership do not change before
publication. The capacity setter and whole-header aliases join the existing index/heap borrow
barrier. This is allocation authority, not an externally usable Graph transaction token.

**Selected first code slice:** ignored Draft `plans/0340-prepared-paged-map-edits.md`, created after
inspecting the plan namespace through0339. Its subsequently authorized map-only
[execution is blocked](#map-owner-execution-checkpoint), not a completed protocol implementation.
Scalar and batch paths share one image engine, but header-codec normalization and IC acceptance
remain open. The batch API is required because the free-span owner cannot prove
ordered directory/page edits itself. No LARA raw page geometry or prospective allocator should be
implemented before that owner can seal its own edits. This is the first dependency of the selected
protocol, **not** another admission repair or a complete A0 slice. Plan0339 stays completed.

The revised Draft specifies one baseline-compatible scalar oracle plus four batch witnesses, four
new compile-fail examples and the existing doctest, normal scheduling, two mutation sensitivities,
a direct scalar IC pair and retained allocator split/restore pair. Its **600s /120s protected final
reserve** was authorized without expansion; the recorded execution failed its IC gates. It does not
budget full span guards,
heap planning, mixed-origin discard/retry, late peers, nested replan or whole-unit pricing. A second
independent approval is needed before those later source owners are edited; no unspent admission
time can be used for them.

| Design axis | Boundary judgment / remaining proof |
| --- | --- |
| Encapsulation | Map images stay private to the map; free-record/bin/heap images stay private to the allocator; capacity backing/serialization stay in the slab/init owners. No LARA directory arithmetic. |
| Separation of concerns | `span.rs` combines allocation and capacity intent; `compact.rs` alone chooses placement and old physical ownership. The plan does not infer retirement from logical degree or target values. |
| Invariants | Actual borrows exclude competing allocator/capacity mutation; consuming draft steps discard on error. Whole-A1 metadata cannot escape the closed synchronous owner call. Compile/runtime proof remains required. |
| Consistency | One scalar/batch image engine and one prospective coalescer replace old publishing branches. No inverse updater, epoch, second allocator or duplicate state schema. |
| Fitness and cost | Forward-only preparation avoids cancellation's second publisher. It still adds touched-image scratch; mixed maximum selection has a real warm frontier/cold-heap cost, not constant work. Each owner must be measured. |

**Recommendation:** review the blocked map candidate and failed gates before any further implementation. The
allocation/cancellation *choice* is resolved in the proposal; lifetime/type correctness, complete
alias coverage, warm/cold behavior, exact errors and performance are unproved. A1 retains its late-peer,
early-pin, final-rewrite and streaming/overlap obligations below. No claim that the map slice alone
supplies nonfailing acquisition return, native retirement, reclamation progress or Graph atomicity.

#### P2 scalar cost revision

**Historical prose checkpoint; current execution status is above.** At this checkpoint no regression
was asserted or measured. The missing gate was that whole-page images/per-scalar
vectors could pass functional tests and both old allocator controls. The ADR now pins
[field/range granularity and inline scratch](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#scalar-dirty-write-and-scratch-contract):
value-only replacement, nonwriting absence, length/entry-only tail edits; split/directory work alone
materializes its affected ranges. One edit rule emits both inline and overflowing images, not two
editors. Zero/one results stay inline; only overflowing bytes/ranges or multiple results reserve.

Draft0340 pins a97-entry public-seeded fixture, a same-source scalar test with explicit traffic
ceilings, and candidate owner-local scratch observations (baseline unobserved, not invented zero).
One narrow new map benchmark prices replacement, absence, tail pairs and actual split/directory
shift separately on the same fixture. Future Rust scope explicitly includes that addition in
`src/bench.rs` and the measurement-only test wrapper/profile/seeder in `lib.rs`. No new broad
benchmark series or graph matrix. Existing mixed-map benches are correctly classified as tail
insert/remove, not replacement. The largest-bin pair is replaced, not appended: pre-final475s +5s
contingency, final120s. All accepted tests and all four batch/borrow obligations remain.
The corruption witness now poisons only page2's flags and requires edit0 staged before edit1 reaches
the shared selected-page ACTIVE check; an earlier guard is not substitute evidence.

P2 provenance is separate under `$TMPDIR/spill-scalar-design-f297-7_msi5x1/`, preserving the reviewed
40-file acquisition root and original failed reference gate. Revised
[Draft snapshot](sources/adr0097-acquisition-scalar-plan.txt),
[structural receipt](sources/adr0097-acquisition-scalar-check.txt) and
[prose checks](sources/adr0097-acquisition-scalar-prose.json) do not replace earlier receipts.
No Rust/benchmark execution, source edit, lock/ref replacement, commit or merge occurred here.

#### Four full-A0 obligations, deferred rather than weakened

| Proposed test / existing seam | Minimum observable result |
| --- | --- |
| `prepared_release_rejects_late_interval` — `test_store()` (`VirtualMemory<DefaultMemoryImpl>`) | Seed `[100,110)`, `[200,210)`; prepare `[20,30)`, then `[105,106)`. Exact retirement-index1 `OverlapPrevious` with previous `[100,110)` / inserted `[105,106)`; headers, bins, records, index and heap unchanged. Early scalar release of the first intent must fail this oracle. Interacting valid releases `[15,20)`, `[20,25)` beside `[10,15)`, `[25,30)` must produce exactly `[10,30)`, not independent preflights. |
| `prepared_spans_origins_discard_and_retry` — one existing edge-store type over `FailpointMemory` | Replace the old release-only capacity witness with the ADR's two-origin trace: actual backed free-list destination, genuine slab grow, sealed subsequent retirement, then a labelled final-metadata rejection. Both capacity copies, free ownership and source bytes unchanged; backing growth separately recorded. Explicit discard and exact retry; early free take and early capacity publication mutations must each fail. Reuse this store for actual record/index/slab growth failures and a labelled owner-local heap `try_reserve` failure. Observe precise roles/errors and post-ready reserve/grow sites, not system OOM or global allocation freedom. |
| `prepared_release_success_and_shared_paths` — one `test_paged_store()`, plus the same edge store for capacity aliases | Scalar/interacting batch/prefix restoration share one rule and preserve the fast successor path. A mixed take of the largest span must not retain a stale release-only maximum. Measure cold/warm heap visits/capacity, stale-threshold rebuild and read-only frontier behavior. Alias tests cover every allocator mutator **and** element-capacity/whole-header writers before writes. Compile-fail competing plan/ready and wrong-owner construction/consume. No late epoch check. |
| `prepared_release_directory_and_split_contract` — map owner | Ordered update then full-directory split rejection, later-remove precedence, exact unchanged first value/backing, and remove-first retry are the map foundation's obligations. Its passing source-pinned proof may be reused; a duplicate huge fixture is not required. Map-only success still does not certify free-span coalescing, two-origin discard or A1. |

The revised full-A0 family remains bounded to **two standalone free-span stores plus one edge
store owning the third free-span store, and at most one standalone map** per full pass. This
explicitly adds the edge-store/capacity seam to the older release-only design; zero graphs/PocketIC.
The new map Draft budgets only its map tests, not this family or the earlier r2 three-store proposal.
No full-A0 names may be reported green from admission/map-only filters; source suspects and
uncompiled proposed APIs are not runtime reds.

#### Implemented bounded repair — non-destructive allocator admission, not complete A0

The two-file repair in ignored **plan0339** is implemented as reported above. The following records
its accepted scope; it adds no prepared capability, public API/error, guard, new module or dependency:

1. In `free_span.rs`, share exact-record lookup/validation through `exact_span_record` for pair admission,
   `replace_existing_span` and `remove_span_exact`; validate before removing from the index.
   Delete remove-then-reinsert-on-mismatch. In `replace_exact_pair_with`, reject identical resolved
   records before the first removal, preserving its current `MissingSpan { span: right }` and
   earlier replacement-validation precedence. Also reject an unrelated existing replacement key
   with the current `DuplicateStart` before removing either input. Strict neighbor checks alone
   miss that equal-key case. Delete superseded exactness checks; do not cache stale bin links across
   removals. These are admission repairs, not atomicity of valid pair replacement or subsequent
   index/heap failures.
2. In the paged-map owner, make `reserve_for_inserts(0)` a nonwriting no-op and avoid any sentinel
   write when the required backing is already present. The inspected base unconditionally wrote zero
   at the last byte of the last allocated page for zero reservation; a full page's final `u64`
   value can occupy that byte. Preserve positive-request sizing and its `GrowFailed` surface;
   current sizing is saturating, not a checked-capacity proof. Positive reservation remains
   growth-only, never a promise of directory admission.

At r2 these were source-derived candidates, not runtime reds. The separately authorized execution
now distinguishes the reached failures and already-safe controls above, under the same 600s ceiling
with 120s final reserve. The repair did not grow into prepared allocation/retirement and remains
independently useful even if complete A0 never lands.

### A1 — whole outer physical publication, only after A0 review

A1's source surface is `graph/compact.rs`, `graph/leaf_pin.rs`, `labeled/bucket_store.rs`,
`lara/edge/row_layout.rs` and `lara/edge/span.rs`; existing fixture access in `graph/test_support.rs`
requires a specifically reviewed minimal seam, not fixture changes. A0's span/capacity, free-span
and map owners are reused, not reimplemented. No native writer, removal, value-store, Graph or
deferred-queue edit belongs here.

The [ADR outer-unit table](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#physical-ownership-and-the-outer-publication-unit)
is the execution contract. One private prepared physical layout owns all participants' final bytes,
original source copy ranges, planned destinations, old/new pin, final counters, the bound combined
acquisition/retirement image and raw scratch. Prepare the entire leaf for each relocation, and include the final
requested rewrite if base resolution requires relocation. Delete nested publish/replan and replace
the old 12-argument commit plus all three call sites with one prepare/consume boundary. Do not put a
safe vertex helper inside the same unsafe outer loops.

For disjoint destinations, prove union-level disjointness and reserve one reusable buffer of
`max_v(P_v)` physical bytes; then stream peers without late allocation. For overlap, read all
endangered source bytes before publication (whole affected payload `sum_v(P_v)` is allowed).
All descriptors/addresses/counters/releases are prepared in **both** arms. Source validation is
metadata/range validation, not a second decode/scan of unchanged payload. Publish has no observer,
fallible re-encoding/replanning or new allocation. Old pinned cover is retired once, never once per
peer; origin-only movement cannot generate logical slot events.

**Decisive late-peer witness:** use a real leaf with at least two relocating peers, an independently
valued normal/bypass neighbor and combined tree-root region. Force a rejection in final-peer
descriptor/count/destination preparation after earlier peers and a real unpublished target have
been prepared. Assert that the injected/invalid request site was reached; a preparation-entry failure
is not enough. Record every peer's raw initialized ranges, complete bucket/vertex rows, pin, leaf
actual/total/global counters, outside sentinels and active/free ownership at entry. On error they
must remain canonical and all new ownership must be returned; only unreferenced backing growth or
unused target bytes may differ. Relax only the fault and require one exact successful retry.

Run the witness in disjoint and overlap flows, and in rewrite-triggered relocation (its late failure
must occur in the requested **final rewrite**, after peer planning). A test-only final-peer fallible
reservation hook is admissible but must be labelled; real cap failures are separate controls.
The required mutation moves final-peer preparation back after first-peer publication, including the
early-pin variant. It must fail the oracle with an earlier peer/pin changed. A second mutation
restores nested relocate→publish→replan and must fail the rewrite case. These are future sensitivity
obligations, not claimed old-source runtime reds. Root bytes surviving a test do not alone prove pin
or allocator ownership.

Revise the four A1 witness families rather than layering another fixture matrix: late-peer/outer
rejection; normal/bypass retained planes; tree/pin ownership; unpublished reservation return/retry.
At most **six native LARA graphs per full A1 witness pass**; reuse them sequentially for arms and
fault sites after exact restoration. If those paths cannot be forced with this ceiling, revise the
fixture plan before expanding it. Native retirement/empty conversion is not exercised here.

A1 measurement begins at the **outer call**, including preparation, any nested placement work,
copying, publication and retirement; report subphases separately without adding inclusive scopes.
Measure both arms' bytes/calls/growth, scratch high-water (`Q` versus `P`, encoded descriptors and
allocator term) and unchanged rows/runs/roots. Require paired unchanged existing controls
`tiny_relocate_mixed_leaf` and `bench_l_ins_mv_l32_2048`, with source/path admission checked first.
The former forces slab/tiny relocation on one vertex (no tree or late peer); the latter performs
round-robin insertion over 32 vertices, not isolated publication pricing. Native owning fixtures must reach both arms and late
rewrite; if the chosen IC controls miss an arm, report that coverage limit and review the missing
measurement before approval. No correctness pass waives a metadata, payload-copy or heap regression.

## Staged implementation and tests — full stages remain planned

This is a dependency-ordered proof map, not authorization to implement every row in one budget.
Plan `plans/0339-prepared-edge-span-publication.md` (gitignored) now records the implemented
**reduced admission repair only**, not complete A0 or automatic A1 execution. Its
[original snapshot](sources/adr0097-demotion-plan.txt) and [original structural check](sources/adr0097-demotion-plan-draft.txt),
then [r1 snapshot](sources/adr0097-demotion-plan-r1.txt) and [r1 Draft check](sources/adr0097-demotion-plan-r1-draft.txt),
remain historical. The [interface-checkpoint snapshot](sources/adr0097-demotion-plan-r2.txt) and
[its Draft check](sources/adr0097-demotion-plan-r2-draft.txt) identify the then-proposed admission
slice, not the current acquisition decision. The new single Draft is0340; structural validity is
not approval. Do not enable property deletion until
A1, the reclamation decision, B and the empty-retirement gate are reviewed; C is a separate runtime slice. D/E must be independently scoped/authorized and cannot be
credited from native tests. GAP002/004/005/007 and general performance/space/landing remain open.

| Stage | Existing owner, decisive oracle and sensitivity | Proposed fixture cost / exit gate |
| --- | --- | --- |
| **A0 — Prepared allocator resources** | Selected nonpublishing acquisition/capacity plus subsequent retirement under one authority; exact errors, no expected resource work during publication. Scalar/batch paths share their owners. | Not implemented. Draft0340 proposes only the map foundation (scalar work oracle +4 batch tests). Later full A0 needs 2 standalone free-span stores +1 edge store owning a third free-span store, and at most1 map. Approved plan0339 cannot close this row. Zero graphs. |
| **A1 — Whole-unit physical publication** | All peers, pin and final requested rewrite prepared together; decisive late-peer and nested-replan rejection/sensitivity described above. Raw holes/runs/roots, counters and returned ownership are independent oracles. | Separate authorization after A0; four witness families / ≤6 graphs per pass, existing types/fixtures. A0 is not this proof. |
| **B — Native conversion preparation/retirement** | After the policy/work gates: one aligned edge/value selection, approved target and failure dispositions, complete combined-root inventory, empty-resource cleanup, exact target descriptor/cover. Preparation must not tombstone source. Test wrong-value-position mutation, omitted property/interior retirement, and aliasing a second leaf to the first. Each negative test must reach the intended check on valid initial backing. | Reuse existing width0/8/33 edge types. One store-level multi-depth root inventory fixture covers physical levels without millions of graph insertions; a valid synthesized rooted tree is not public promotion reachability. Extra construction budget proposed below. No public assertion removal until review. |
| **C — Public native deletion** | Replace delete→ignored demote with prepared outcomes. Canonical Live/Tombstone/Missing admission, exact removed edge/value, forward/reverse/no-move/move wrappers, same slot/label, both policies. The preserved 513→512 witness defines the red; new final fixtures prove the proposed contract without rerunning that diagnostic now. | Six cases = widths 0/8/33 × Insertion/Unordered. Each uses one existing deferred pair with both orientations populated (12 native LARA graphs total), not 12 specializations. Zero PocketIC. Existing no-op/read fixtures are reused unchanged. |
| **C — Occurrences and observers** | Seed nonzero identities/values per occurrence, including equal-neighbor multiedges; do not derive expectations from final traversal or from target alone. Resolve counterpart before source deletion; assert removed pair and shifted future live rank. Maintain an external `(orientation, owner, label, original slot) → identity` map changed only by actual events. A position-preserving conversion emits zero events; any separately approved movement-bearing operation emits every actual move once, preserves order under Insertion, and checks both policy permissions. | Reuse the same six pairs, neighboring label and owner; include an asymmetric valid hole layout so equal rank is not equal physical slot. Test exact pre/post neighbors/counts, returned values and independent property boundaries (512 entries for width8, 124 for width33). Native paired calls are still two operations, not Graph atomicity. |
| **C — Sparse extent, boundaries and empty** | Decision-pinning cases include `S=1025,D=512` with ample capacity, the approved prefix/run envelope, and larger historical extents. Assert the separately approved reclamation/timing contract, not permanent retention from `S>1024`. At 1024 any no-move conversion preserves holes. Append preserves survivors; repromotion/maintenance behavior needs the approved target contract. Exercise first/last live, interior dead, end/MAX, wrong label, repeat, then final retirement before normal pruning; an already-dead retry produces no work. | One extra width33 pair (2 graphs) for >1024 extent and partial leaves, beyond the six-case matrix. Reuse it through depletion. Do not allocate a graph per slot or test phase. Raw native tombstone-inclusive controls are checked before conversion; slab live-only oracles after it. |
| **B/C — Capacity, corruption, retry** | Actual bounded allocator failures: before targets, after edge but before value, and after both before metadata/retirement preparation. The approved policy must distinguish known capacity from corruption and specify retry/progress; no retained-success oracle is fixed yet. A synchronous last-delete retirement rejection is pre-write but does not prove liveness. Corrupt roots/counts/free intervals stay errors, not empty success or capacity. Relax only the capacity bound and retry; canonical rows/peers unchanged on rejection, allocated/free disjointness and exact returned resources after cleanup. Reopen with the same backing where fixture support exists. | At most two extra existing capped-backing graphs; vary fault sites sequentially after exact restoration. Prefer store-level allocation failures over another full graph specialization. Compare canonical byte regions separately from allowed monotonic growth/free-list changes. No blanket byte-for-byte allocator rollback claim. |
| **D — Graph integration** | One real Graph fixture with local directed parallel occurrences, canonical sidecars, inline and sidecar postings, neighbor controls and active/building index effects. Delete through the public handle path, verify exactly one logical occurrence, pre-resolved counterpart, surviving handles/properties/postings and maintenance event-only replay. Include reverse-reconciliation's pre-collected no-move sequence. Retain/gate vertex purge until matching GAP004 is safe; do not fix it incidentally. | Propose one reused native Graph fixture plus its existing maintenance harness, not a second width/policy Cartesian matrix. Later authorization must pin the actual test and setup count. Native success tests do not prove rollback of a fence or a partial paired delete. |
| **E — IC transaction evidence** | One pre-publication preparation rejection after Graph's fence and one injected post-forward/pre-counterpart or post-movement observer trap. Assert same-message adjacency, both halves, sidecars, postings, build-DML/outbox and queue rollback, then one successful retry with no duplicate deletion/events. Prior committed ticks must remain committed. | Proposal: 1 PocketIC test, 1 `PocketIc` constructor, 1 Graph canister install, no federation install, sequential failures/retry in that instance. Final harness/dependency inventory and execution budget require separate approval; no claim this setup exists or has run. |

**Construction accounting:** proposed stages B/C add at most 16 LARA graphs (12 matrix + 2 sparse +
2 fault fixtures), plus one store-only multi-depth fixture; repeated assertions reuse those objects.
This is a planning ceiling, not execution evidence, and repeated filtered test executions multiply
cost. The earlier prose-only checkpoints ran none of these stages. The separately authorized
admission execution ran only its four existing IC controls and owner tests, not these future
fixtures, a broad suite or a PocketIC campaign.

The matrix must distinguish pre-write rejection from whatever capacity disposition is separately approved. Controls
must force the actual mode/physical placement; a compile/setup error or a passing unrelated bulk
visitor is not a red. Exact values use full nonzero occurrence IDs and distinctive remaining bytes;
edge targets alone are not injective in the parallel-edge case. Sparse selections, neighbor labels,
other orientation and external identity maps must not be reconstructed from the implementation's
new output. Stop and inspect the owning layer on recurring failure; do not weaken these contracts.

## Review and delivery boundaries

No shrink-policy choice has been approved. The old permanent-retention choice and assertion that
resumable reclamation inherently requires new persistent state are withdrawn. The implemented
two-file admission repair is independently approved at its frozen hashes; complete A0's
checked-edit/nonpublishing acquisition capabilities are not bundled. A1 must still pass whole-unit late-peer rejection and performance
gates. Neither slice approves tree retention,
native conversion/deletion, empty-retirement liveness, Graph fences or landing.

The [ADR empty-retirement bounds](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md#empty-retirement-liveness-and-work-gate)
follow `S` and both stream depths. At the u32 extent envelope, widths0/8/33 imply at least
16,793,616 / 50,380,848 / 155,476,392 bytes for flat block-ID inventories alone. These arithmetic
bounds are not validated large fixtures or an IC instruction allowance. A separately reviewed bounded
retirement design is required wherever synchronous work/scratch cannot be proved admissible. Do not
reduce a cap, orphan roots, or declare indefinite last-delete rejection an acceptable completion.

The original/r1/r2 checkpoints were prose-only. Subsequent authority covered only the reduced
admission execution above, never the full stages or prior unused balances. No further Rust execution
is authorized by closing this plan. Protected source, lock, benchmark artifacts, foreign files and
prior failure evidence remain unchanged. The earlier selected-read administrative timeout and
historical failed validations retain their original provenance.

### Acquisition checkpoint provenance

The present acceptance/status changes and acquisition decision have separate provenance under
`$TMPDIR/spill-acquisition-decision-f297-mcworwje/`; its `before/` copies preserve the prior report,
ADR0088 and completed plan0339, and `start.json` pins source/status/reference/history hashes.
The old admission `completion.json` still says review pending: it describes that execution's closure,
not this later verdict. No executed artifact or prior manifest was rewritten to manufacture approval.
The [new Draft snapshot](sources/adr0097-acquisition-plan-draft.txt),
[its structural receipt](sources/adr0097-acquisition-plan-check.txt) and
[content-only check metadata](sources/adr0097-acquisition-prose.json) identify this checkpoint.
Those checks do not compile the signatures, run the proposed budget or independently approve it.
The first content verifier stopped at its main-ref assertion: the reflog records a shared-main commit
at `2026-09-23T03:23:06+09:00` to `8b63042c68e5981e3c7afa04c2d3660485e5ce16` outside this
checkpoint's commands. `reference-drift.json` and `initial-check/` preserve the observation, failed
verifier/log and first Draft receipt (0.479s). Subsequent checks report file/content integrity
separately while leaving the original reference gate failed. Main was not reset or silently repinned;
the revised Draft only discloses the observation and requires fresh reference confirmation.
A subsequent whole-ADR link check reached unchanged references to unavailable older plans; the
full diagnostic also found other inherited unavailable targets/one fragment. The failed collector
and diagnostics remain in `link-check-failure/` and `link-diagnostics.json`. The corrected checker
checks changed URLs/targets and records inherited reference debt separately; it does not rewrite
unrelated historical links or count them as valid. No Rust was run for either prose-check correction.

### Prose checks and preserved history

The remainder of this section describes the immutable original/r1/r2 checkpoints, not the current
admission implementation, its later bounded approval or the new Draft. Their historical pending
status is preserved.

The reviewed 12:00:21 draft and its original passing structural check remain in
`$TMPDIR/spill-demotion-design-f297-i2l1uy33/`; its manifest and administrative filename/whitespace
corrections are unchanged. A passing structural check did not preclude REQUEST CHANGES.

Before revision, byte-for-byte copies of the reviewed documents/plan were saved under
`$TMPDIR/spill-demotion-revise-f297-32aisjkk/reviewed/`, with hashes in `start.json`. The old durable plan
snapshot/check are not overwritten. New `-r1` artifacts identify the revised plan. Source-bound size
arithmetic is recorded separately in that revision root, not as execution evidence.

Revision content/link/scope/whitespace checks and the revised Draft structural check passed. All
four TODOs remain pending and all six criteria unchecked. The checks verified 13 protected paths,
10 preserved reviewed copies, the old 13-file design manifest and the prior selected/point/deletion
manifests (115/150/53 entries). The initial revision checker expected the literal phrase "three
free-span stores" rather than "three reusable free-span stores"; that checker-only correction and
its failed output are preserved. An ADR0097 exact replacement also initially missed indentation;
it made no change and was corrected after reading the actual text.

No Rust edits/executions, benchmark, saved-witness rerun, assertion removal, process management,
commit or merge occurred in r1. HEAD/main and the foreign files remained unchanged. Its later
acceptance was restricted to further interface design, not implementation or policy approval.

For this interface checkpoint, exact r1 files were preserved under
`$TMPDIR/spill-a0-interface-f297-jtjersgv/r1/` before editing; `start.json` pins that history, protected
paths, source owners and prior manifests. New `-r2` snapshot/check files do not overwrite r1.
Only the existing report/ignored plan and these two provenance artifacts change from r1. The source
inventory records 25 free-span and 10 map unit tests, not executed test results. The new admission
candidates, full-A0 obligations and future 600s command proposal are **not run**. This checkpoint
still requires review and separate implementation authorization.

The final r2 Draft structural check passed (exit 0, 0.085s), bound to plan/snapshot SHA256
`df3d373470d8d1ef3fe3be0bd3cb99eabac54e22af911382970b75a18845199d` in
`plan-r2-draft-result.json` under that raw root. The first successful check (0.116s, plan hash
`308a29cdd65115c60d99a874a3857fb2da6262f3a4864191a82765798ef50736`) remains in
`initial-check/`; the second followed an explicit per-invocation foreign-CSV preservation note and
mutation wording correction. Generic validator stdout is not interchangeable between plans or
evidence of design approval. Four TODOs remain pending, six criteria unchecked.
Content/scope/hash/whitespace checks passed: 31 relative links/fragments, 13 protected hashes,
12 preserved r1 copies, four source-owner hashes, and unchanged 27/13/115/150/53-file prior
manifests. The proposed command caps sum to 460s plus 20s diagnostic contingency plus 120s final
reserve, not spent execution time. These checks do not approve the interfaces or candidate repairs.
