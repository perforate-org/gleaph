# Lean fixture migration classification and freshness findings

Anchor timestamp: 2026-11-09 08:30:00 UTC +0900
Target revision: git `1159e777dc1b7df4c711b2ea27d8352519320a36`
Status: analysis record feeding [ADR 0104](../adr/0104-verus-owns-implementation-proofs.md),
plan 0370 (Verus intake harness) and the independent verus-ic project's plan 0001 (four gates). It changes no
production code, no Lean fixture, and no CI gate.

## Question

ADR 0104 gives implementation-level proofs to Verus and keeps Lean for the
abstract/model layer. This record answers the concrete question the ADR leaves
open: **for each existing Lean theorem cluster, is it an implementation
transcription that Verus should own, an abstract-model property that Lean should
keep, or a model of code that no longer exists?** It also records the drift that
motivated the decision.

## Measured drift

All three "permanent fixtures" are stale at the target revision. The table lists
the pinned revision recorded in each `SCOPE.md` and the diff of the transcribed
crate's `src/` since that pin.

| Fixture | Pin | Drift since pin | Verdict |
| --- | --- | --- | --- |
| `crates/ic-stable-lara/formal` | `dbd76f3d` (2026-08-26) | 96 files, 65,484 insertions / 31,959 deletions | Stale; Stage 1 codec/bounds still exist but several transcribed checks changed |
| `crates/ic-stable-vec-deque/formal` | `d80cc4c6` (2026-08-26) | 4 files, 624 insertions / 211 deletions | **Obsolete**: ADR 0086 replaced the contiguous ring with the V1 segmented block-ring, implemented the same day; the model describes deleted code |
| `crates/ic-stable-linear-hash-map/formal` | `0da342d6` (2026-08-24) | 2 files, 47 insertions / 11 deletions (`control.rs` shows 54 changed lines) | Stale; the control/routing transcription needs re-anchoring before P3–P5 are trusted |

Two mechanisms cause silent staleness:

- Only the linear-hash-map fixture is CI-gated, and the gate runs `lake build` on
  the pinned model; it never compares the transcription with the current source,
  so a stale model stays green.
- All three `SCOPE.md` files still say "No CI wiring yet (repository has no
  `.github/workflows/`)", but `.github/workflows/` exists and contains
  `lean-verify.yml`. The "run as a permanent fixture" claim is therefore
  unenforced on two of three crates.

Concrete instance: `ic-stable-lara` pinned `dbd76f3d` contains
`if index < DEFAULT_MAX_LOG_ENTRIES`, while current `log_head.rs` uses
`if index < Self::MAX_VALID_INDEX` (`169`), so index 169 is now rejected by the
implementation though the module documentation, `from_i32` documentation and the
Lean model all describe it as valid. The Rust unit test
`assert_eq!(from_i32(169), from_index(169))` passes because both sides are
`None`, and `slab_index.rs` turns the rejection into
`expect("overflow log head out of range")`. The Lean model is not wrong about
the pinned revision; it is wrong about today's code, and nothing detected it.

## Classification method

Each layer was classified by what its theorems are about:

- **Transcription** — properties of functions that still exist in the crate
  (codec round-trips, bit packing, bounds, header validity). These are exactly
  the properties that drift, and Verus can state them on the shipped functions.
- **Model** — properties of an abstract algorithm or state machine, deliberately
  independent of the concrete layout (list specification, split preservation,
  epoch fencing, failure atomicity). These need structured induction and are
  worth keeping kernel-checked.
- **Obsolete** — properties of code that no longer exists.

Verus migration is recommended only when the property is tied to shipped code,
the code is pure/safe or Verus-expressible, and the arithmetic is
SMT-friendly (linear/bitvector). Lean retention is recommended for hard
induction or number theory, layout-independent models, and where the
kernel-checked independence is itself the assurance.

## `ic-stable-lara`: 66 theorems, all transcription

| Cluster | Files | Theorems | Verus migration |
| --- | --- | --- | --- |
| LogHead codec (P5 sentinel, round-trip) | `Lar/LogHead.lean` | 16 | Yes — priority 1 (drift already observed, 91-line module) |
| 36-bit slot index, u40 offsets | `Lar/SlotIndex.lean` | 16 | Yes — checked arithmetic and bounds, bitvector-friendly |
| Locator word, tail28, packing basics | `Lar/Basic.lean` | 16 | Yes |
| BucketLabelKey identity/directedness | `Lar/BucketLabelKey.lean` | 9 | Yes |
| Bucket word packing/non-interference | `Lar/BucketWord.lean` | 7 | Yes |
| LabelBucket fail-closed validation | `Lar/LabelBucket.lean` | 2 | Yes, but the transcription must be re-anchored first (three validation sites changed) |

Stages 2–3 (scan-contract geometry over abstract stores, PMA accounting
preservation) are planned, not verified, and the range-permutation class is
explicitly deferred in `SCOPE.md`. If restarted, they are model properties and
belong to Lean.

## `ic-stable-vec-deque`: 41 theorems, model of a deleted layout

| Cluster | Files | Theorems | Verdict |
| --- | --- | --- | --- |
| Ring arithmetic (extent, injection) | `Svd/Ring.lean`, `Svd/Basic.lean` | 13 | Obsolete as written (contiguous ring); the segmented-ring restatement is a new implementation-level obligation, natural for Verus once the layout stabilizes |
| Header validity | `Svd/Header.lean` | 1 | Obsolete (`DATA_OFFSET` and `HeaderValid` changed per ADR 0086) |
| Abstract list specification and preservation | `Svd/Abs/*` | 27 | Model; ADR 0086 §Formal-layer impact says these are layout-independent and carry over — keep in Lean, restated |

Do not migrate this fixture as-is. The correct next action is restatement for
the V1 segmented block-ring, split by whether each property is header/block
arithmetic (Verus candidate) or list-level semantics (Lean).

## `ic-stable-linear-hash-map`: 187 theorems, mostly model

| Cluster | Files | Theorems | Verdict |
| --- | --- | --- | --- |
| Routing math (P1 route extent), geometry (P2/P3), control validity (P4), threshold monotonicity (P5) | `Lhm/Routing.lean`, `Lhm/Basic.lean`, `Lhm/Control.lean` | 25 | Mostly model (hash opacity, geometry); `linear_bucket` bounds are a small Verus candidate but low priority. Re-anchor `control.rs` first |
| Logical map spec, transfer, search, placement | `Lhm/Abs/*` (Base, Transfer, Search, Place, Deltas, Preserve, OpPreserve, Ops, State) | 87 | Model — keep Lean |
| Split and split-with-insert preservation | `Lhm/Abs/{Split,SplitInsert}.lean` | 44 | Model — keep Lean (repacking and candidate routing, structured by case) |
| Epoch fencing and failure atomicity | `Lhm/Abs/Epoch.lean` | 30 | Model — keep Lean; kernel-checked failure atomicity is stronger evidence than an SMT proof of the same claim |
| Cleared-state preservation | `Lhm/Abs/Cleared.lean` | 3 | Model — keep Lean |

This is the only CI-gated fixture and the most mature; migrating it to Verus
would be expensive and would trade kernel-checked evidence for SMT-dependent
evidence. Keep it, add a freshness gate, and re-anchor `control.rs`.

## Order and retire conditions

1. **lara Stage 1 → Verus** after plan 0370's harness exists. Retire the Lean
   fixture only when every P1–P6 property is proved on shipped functions, the
   mutant controls show the specs are not vacuous, the proofs run in CI, and the
   loss of an independent transcription is an explicit accepted trade-off.
2. **vec-deque → restate, then split.** Treat the current fixture as obsolete.
   New header/block-ring arithmetic is a Verus candidate; list-level semantics
   stay in Lean.
3. **lhm → keep Lean.** Optional single Verus proof of `linear_bucket` bounds;
   re-anchor and add the freshness gate immediately.
4. **One owner per property.** During a transition the Lean fixture is a
   reference model, not a second gate; no property is proved by both tools
   long-term.

## Freshness gate for surviving Lean fixtures

Proposal, cheap and CI-able: commit a per-fixture manifest (for example
`crates/<crate>/formal/SOURCES.sha256`) mapping every transcribed source path to
the blob SHA-256 at the anchor revision. A CI step reads each entry with
`git show HEAD:<path> | shasum -a 256` and fails when the hash differs, printing
the changed paths. Re-anchoring is then an explicit, reviewable edit that
re-reads the diff and updates the model. This closes the gap that let three
fixtures drift unnoticed.

## Open questions

- Which `control.rs` changes invalidate P3–P5 in the linear-hash-map model?
- What is the restatement scope for the segmented block-ring per ADR 0086, and
  does the list-level Abs layer need changes for the directory/free-list
  invariants?
- Can Verus proofs of lara Stage 1 stay small and fast enough for a path-filtered
  CI job, or do they need `rlimit` budgets recorded per module?
- Does the repo want the freshness gate to be blocking for the surviving Lean
  fixtures, or advisory until the ADR follow-up?
