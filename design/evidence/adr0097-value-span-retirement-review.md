# Value-span retirement review and batch-regression follow-up

**Status: Implemented — batch repair validated; final Standards and Spec re-reviews approve.**

Reviewed commit: `14430cc622394028503702905db42ad11d6ff029`,
`fix(lara): retire replaced value spans after publication`.
Base: `24467b17b375b2a7bc462818ef278553a52993fc`.
Specification: `plans/0371-value-span-retirement.md`, including its subsequent commit authorization.
The [implementation and validation report](adr0097-value-span-retirement.md) retains the completed
runtime evidence; this review does not replace or extend those test runs.

Two independent reviewers inspected the pinned commit in separate read-only sessions. The
implementation/standards axis found no actionable issue. The specification/test axis found one
blocking P2 coverage gap. Repeated-fold performance measurement is deferred until that gap is closed.
No implementation defect was established by that review, and no tests were executed during it.
The Standards/Spec verdicts and next-step text below are the historical 0371 review, not verdicts on
the uncommitted 0372 repair. Its corrected reachability and follow-up are recorded at the end.

## Standards — APPROVE, no findings

The reviewer confirmed that the graph-owned publisher writes the replacement descriptor before
retiring a changed, nonempty source. Scalar admission validates vertex accounting and reserves
retirement capacity before relocation; sufficient spans and in-place growth do not retire live data.
The compactor and all four batch replacement paths use the shared publication boundary.

Common batch preflight reserves capacity for possible value allocations and appended-tail rollback.
No new module, type, flag, dependency, or duplicate replacement rule was found. Existing outer fold
operations were distinguished from the changed publication contract rather than treated as new defects.
This is a static implementation verdict, not a claim that untested failure paths passed at runtime.

## Spec — REQUEST CHANGES, one P2

### P2: batch replacement retirement is not directly protected by the regressions

The plan requires the shared owner to be "used by scalar, compactor and all four batch replacement
paths" and common batch preflight to reserve enough records for "value allocations and rollback."
The new regression tests strongly cover scalar/fold relocation, but the reviewed batch tests do not
observe the equivalent old-span retirement and retirement-reservation failure contract.

Relevant committed locations:

- [`batch_write.rs:1205`](../../crates/ic-stable-lara/src/labeled/graph/batch_write.rs#L1205): common
  retirement-capacity reservation for New, GrowInPlace and Reallocated allocations.
- [`batch_write.rs:3164`](../../crates/ic-stable-lara/src/labeled/graph/batch_write.rs#L3164): shared
  publication after the Slab/SlabHoles commit paths.
- [`batch_write.rs:3448`](../../crates/ic-stable-lara/src/labeled/graph/batch_write.rs#L3448):
  RelocatedSlab publication.
- [`batch_write.rs:3907`](../../crates/ic-stable-lara/src/labeled/graph/batch_write.rs#L3907):
  ExpandedSlab publication.
- [`batch_write_test.rs:270`](../../crates/ic-stable-lara/src/labeled/graph/batch_write_test.rs#L270)
  and [`:313`](../../crates/ic-stable-lara/src/labeled/graph/batch_write_test.rs#L313): successful
  new-span writes/readback do not establish retirement of an existing non-tail source.

**Static counterexamples:** replacing the RelocatedSlab publisher call with a direct descriptor
write could preserve all observed rows while leaking the old span. Removing an allocation kind
from common retirement preflight could remain undetected until free-record capacity is exhausted.
These counterexamples were reasoned from the assertions; no mutation test was run during review.

**Required correction:** extend the existing native batch fixtures to exercise actual nonempty,
non-tail value-span replacement for the reachable Slab, SlabHoles, RelocatedSlab and ExpandedSlab
paths. Assert destination/allocation selection so a fallback cannot mask the intended path. Verify
exact old-offset allocator reuse without tail growth, unchanged live rows after poisoning that
reused source, and descriptor/vertex/free-byte accounting. At the common preflight boundary, inject
retirement-reservation failure and assert unchanged canonical state, plus the promised rollback
capacity behavior. Do not fabricate invalid descriptors merely to enter a branch.

Use the existing graph/store test layer and bounded fixtures. New PocketIC installations, a
production test hook, or a generalized fixture framework are not prerequisites. Re-review must
replay the stated counterexamples against the new assertions before approving this finding.

## Covered contracts and evidence limits

The widths 8/33 scalar witnesses protect two relocations, distinct peers, exact rows in both orders,
edge/run identity, best-fit reuse, poisoning, accounting, sufficient-span retention and tail growth.
The failure witness compares all 16 pre-existing backing prefixes, descriptor, vertex and accounting
across retirement-reservation and destination-allocation failures, followed by successful retry.
These strengths do not substitute for observing the changed batch caller and reservation paths.

The reviewed receipts record three intentional behavioral failures before repair, three focused
passes afterward, and 167 affected passes plus one insertion control. Formatting, lint classification,
complexity and fold benchmark receipts exist. Strict lint, full workspace/library validation,
PocketIC and lifecycle fault injection remain outside those claims.

The validation environment included frozen dirty allocator and benchmark dependencies, as recorded
in the original protected manifest. It was not a clean-checkout validation of the commit. The
Standards review used committed snapshots; its concluding reference to "clean committed dependency"
validation was inaccurate and is corrected here. This does not add a runtime claim to either axis.

## Review receipts and next step

Raw evidence:
`/Users/yota/.local/share/gleaph/evidence/adr0097/value-span-review-14430cc62`.
`standards-v2.md` and `spec-v2.md` preserve the independent reports; their JSONL transcripts and
receipts record read-only commands, normal exit and elapsed time. Each reviewer used 15 read-only
shell calls and completed within four minutes. No reviewer edited source or ran validation.
The initial model selection failed before inspection; those startup failures are retained separately
and are not review results.

The coordinator confirmed the cited allocation paths and existing assertions, and verified that
all 265 protected paths, the four implementation files, HEAD and index remained unchanged.
Only this review record and the implementation report's review-status link are added afterward.
No new commit, push, source/test edit or benchmark run is made in this review step.

**Next:** close the batch regression finding and obtain a focused re-review. Only then start the
repeated append/fold/read experiment. The existing native/benchmark results remain valid within
their stated scope, but the broader shared-boundary acceptance gate was not passed at that checkpoint.

## 0372 reachability correction and repair

The original request overestimated reachable batch destinations. `preflight_run` admits Slab,
SlabHoles or Tree. Both overflow exits return `SpillRunDeferred` unconditionally; the only constructor
of ExpandedSlab is the uncalled `preflight_expanded_run`, and RelocatedSlab derives from that state.
Consequently, a RelocatedSlab-only publisher bypass is not an executable regression under ADR0097 §7.
Those two runtime-test requests are dismissed as unreachable, not silently marked passing or revived.

The [batch regression evidence](adr0097-batch-value-span-regressions.md) records six native tests,
173 affected passes and eight behavioral mutation failures. The witnesses exposed actual Slab prefix
overwrite, missing per-owner byte/relative-cover accounting, and rollback tail/free-list double
ownership. These are repaired at the existing batch owner. With rollback corrected, only
New/Reallocated need release-record reservations; GrowInPlace and tail rewind need none.
The original New/GrowInPlace/Reallocated-plus-tail premise is superseded, with a no-grow control.

The first fix re-review found a further P2: partial-allocation compensation asserted aggregate free
statistics, not the exact free-span list. Returning a consumed prefix at the wrong offset passed the
old oracle. Two test-only lines now compare the full list; the same mutation fails, and all 173
affected tests pass again. Those review launches exceeded their cap and could not finish receipt
inspection, so their late reports do not constitute approval.

## Final independent re-review — both axes APPROVE

Final reports in the 0372 evidence root are `review-standards-v4.md` and `review-spec-v4.md`.
Each reviewer completed 10 file-only read commands, with exit 0 in 129.34s / 104.64s. Neither ran
validation or delegated. Earlier flat attempts encountered macOS Git temporary-cache creation;
they are not acceptance receipts. The final reviewers avoided that launcher and inspected verified
base snapshots, current files, raw evidence and hashes directly.

- **Standards — APPROVE, 0 findings:** shared publisher, per-owner accounting and relative cover,
  single rollback owner, deleted duplicate accounting, bounded helper/API growth and active-doc sync.
- **Spec — APPROVE, 0 findings:** all reachable width/path witnesses and exact error/postcondition
  assertions; the wrong-offset restoration passes the old oracle and fails the new exact-list check.
  Final source/tested hashes, eight runtime mutations, 173 passes and benchmark/lint limits agree.

The original 0371 batch P2 and the subsequent partial-rollback-identity P2 are closed. The corrected
unreachable-path request remains dismissed, not counted as runtime coverage. Final manifest and
plan receipts are linked from the [0372 evidence](adr0097-batch-value-span-regressions.md).
The subsequent user request authorizes committing only the reviewed repair. Repeated-fold
experiments remain a separate slice, and no automatic-fold threshold follows from these results.
