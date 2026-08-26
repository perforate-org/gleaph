# Verification Report — ic-stable-linear-hash-map, Stage A + Stage 3 + Stage 4

Anchor timestamp: 2026-08-24 16:33:21 UTC +0000
(stage 3 addendum anchored 2026-08-25 03:17:05 UTC +0000)
(stage 4 addendum anchored 2026-08-26 02:14:56 UTC +0000)
(stage 5 addendum anchored 2026-08-26 04:44:01 UTC +0000)
Target revision: git `0da342d62b2a3c3b293fa7ff5ed21b9f577dd23d`
(`crates/ic-stable-linear-hash-map/` clean at this revision)
Mode: audit of an existing implementation, run as a permanent fixture (see SCOPE.md)

Post-audit remediation (anchored 2026-08-24 21:31:38 UTC +0000): findings 1 and 2
were fixed by consolidating the level/cursor derivation into control.rs
`derive_geometry`. Line citations for the affected functions were refreshed to the
post-remediation source; all other citations still match revision `0da342d62`
(map.rs line numbers are unchanged by the remediation).

## How to run

```
cd crates/ic-stable-linear-hash-map/formal
lake build
```

`lake build` re-checks every proof. The root module prints `#print axioms` for each
headline theorem; a regression to `sorry` appears as `sorryAx` in that output. The
project uses Lean core only (`leanprover/lean4:v4.33.1`, no Mathlib) and is invisible
to Cargo.

## Scope

Stage A of the staged roadmap in SCOPE.md: routing mathematics and control-region
invariants (properties P1–P5), plus — since the 2026-08-25 addendum below — stage 3,
the abstract logical map layer (`Lhm/Abs/`), — since the 2026-08-26 addendum below —
stage 4, split preservation (`Lhm/Abs/Split.lean`), and — since the second
2026-08-26 addendum below — stage 5, epoch fencing / failure atomicity, the
write-before-publish ordering, and the retry-loop progress measure
(`Lhm/Abs/Epoch.lean`).

## Method

Hand-written Lean transcription of the referenced Rust functions, each citing its
source file and line range at the target revision, followed by proofs of the P1–P5
statements over those transcriptions. The Rust→Lean generators (Aeneas/Charon, hax)
were evaluated and not used: the crate's load-bearing paths are impure (`Memory`
trait I/O, thread-local scratch, external rapidhash), so automated extraction would
cover the wrong layer. Revisit if a pure-function cross-check is ever wanted.

## Verified results

| Property | Lean theorem | Mirrors |
|---|---|---|
| P1 route extent | `route_lt_base_plus_cursor` | map.rs L1963-L1971 `linear_bucket` |
| P2a split stability (level step) | `split_stability_level_up` | geometry transitions of map.rs L1693-L1709 |
| P2b split stability (cursor step) | `split_stability_cursor_adv` | geometry transitions of map.rs L1693-L1709 |
| P3 geometry-step validity | `next_geometry_shape` (+ `next_geometry_from_valid`) | map.rs L1693-L1709 `next_geometry`, L1719-L1721 `base_buckets` |
| P4 initial control validity | `initialControl_valid`, invariant set `ValidControl` | map.rs L401-L415 `create`, L1047-L1068 `validate_control`, header.rs L45-L56 |
| P5 threshold bounds | `split_threshold_mono`, `split_threshold_le_capacity`, `split_threshold_lt_capacity` | map.rs L1711-L1717 `split_threshold` |
| Corollary | `route_in_extent`: routed buckets < `physical_buckets` under `ValidControl` | composes P1 with P4 |

Notable strengthening discovered during proof work: P1 holds **unconditionally** — no
`cursor < 2^level` hypothesis is needed for the bound itself. That hypothesis is only
required to identify `2^level + cursor` with the persisted `physical_buckets`, which
is exactly what `ValidControl` provides.

## Stage 3 addendum — logical map layer (anchored 2026-08-25 03:17:05 UTC +0000)

Stage 3 verifies the abstract logical map (`Lhm/Abs/`) on top of Stage A. All
headlines below depend only on `propext` / `Quot.sound`; a `#print axioms` block in
the root module re-checks this on every build.

| Result | Lean theorem | Content |
|---|---|---|
| Transfer core | `inv_transfer_core` | `Inv` inherits across any state update sharing hashes/geometry/incarnation, with direct new-state counter equations and placement/uniqueness facts relative to the old state |
| Occupancy-preserving case | `inv_transfer` | specialization deriving counter transport from pointwise occupancy agreement; used by the insert-update path |
| Insert-update preservation | `inv_setValue` | overwriting an existing same-key slot preserves `Inv` |
| Insert-place preservation | `inv_place` | placing a fresh entry into a free slot of a candidate bucket preserves `Inv`, given global key absence from both candidate blocks |
| Remove preservation | `inv_clearSlot` | clearing a genuinely occupied slot preserves `Inv` (no candidate facts needed) |
| Clear / reset preservation | `inv_cleared`, `inv_reset` (+ helper `inv_set_incarnation`) | clear/reset restore a pristine, `Inv`-satisfying surface |
| Top-level contracts | `opInsert_preserves`, `opRemove_preserves` (+ result-state lemmas, `chooseFreeSlot_spec`) | full semantic insert/remove preserve `Inv` in every outcome branch |

Modeling decisions recorded during stage 3:

- **Cleared state is a full logical wipe.** The earlier draft kept stale entries
  beyond the initial extent in `clearedState`. That is unprovable as an `Inv`
  instance (stale slots violate `placed`) and unnecessary: no modeled operation can
  observe those slots, because reads stop at the published extent and growth republishes
  only fully rewritten blocks (REPORT.md finding 4). `clearedState` therefore maps
  every flattened slot to `none`, and finding 4's byte-level write-before-publish
  ordering stays a stage-5 obligation.
- **Axiom hygiene fix.** `cand_lt_pb` originally depended on `Classical.choice` via a
  single `omega` call juggling two routing-if atoms. Splitting the conjunction into
  two independent `omega` closers removed the dependency; every stage-3 headline now
  rests on `propext` / `Quot.sound` only.

## Stage 4 addendum — split preservation (anchored 2026-08-26 02:14:56 UTC +0000)

Stage 4 verifies `inv_split_transfer` in `Lhm/Abs/Split.lean`: one successful
maintenance split — map.rs `plan_split` L1453-L1557 with `insert = none`, published by
L1668-L1675, counters recomputed as in `finish_split_plan` L1575-L1620 — preserves
`Inv`. The headline depends only on `propext` / `Quot.sound` (`#print axioms` in the
root module re-checks this on every build).

| Result | Lean theorem | Content |
|---|---|---|
| Step shape | `nextGeometry_cases` | a successful `nextGeometry` (map.rs L1693-L1709) is exactly a cursor advance or a frontier level increment; both grow buckets by one |
| Routing fixity | `route_fixed_step` (+ `_adv` / `_up` / `_high_*` variants) | off the source bucket, every entry keeps both candidates across the step; on the source, P2 gives `{source, source + 2^level}` |
| Destination choice | `splitDest`, `splitDest_cases`, `splitDest_disjoint`, `splitDest_defined` | mirror of map.rs L1476-L1483: source when either candidate hits it, else new bucket, else fail-closed `none`; every old-source entry has a destination |
| Transformer | `splitState` over `splitSrcFun` / `splitNewFun` / `splitBuckets` / `splitOverflow` | source block re-packed to `{source, source + base}` via `packImg` (map.rs `entries_from_image` + `append_entry_to_image`), everything else copied verbatim |
| Load partition | `part`, `lsrc`, `lnew`, `partL` | the two re-packed images' loads sum to the old source load; each image's occupancy equals its selection count |
| Counter recomputation | countersLen / countersOvf bullets of `inv_split_transfer_aux` | `len = Σ loads` and `overflow_entries = Σ overflow-loads` over the grown extent, using per-image identification `ovfLoadOf s2 b = ovfCountFun image` and `totalLoads_except` |
| Placement transport | `placed` bullet | every new-state entry lies in a real slot of a candidate block under the stepped geometry |
| Key uniqueness | `unique` bullet | global key uniqueness transports through re-packing; cross-image same-key collisions are excluded by `splitDest_disjoint` |

Modeling decisions recorded during stage 4:

- **Overflow accounting needs no partition identity.** An early draft postulated
  `ovfCountFun srcImage + ovfCountFun newImage = ovfLoadOf oldSource`. That claim is
  **false** in general: re-packing compacts entries into freed primary slots, so the
  packed images' overflow counts do not sum to the old block's overflow count whenever
  the split distributes entries across both destinations while the old block had
  primary-slot holes. The Rust code never asserts such an identity either —
  `finish_split_plan` simply subtracts `source_old_overflow` and adds each rewritten
  image's own `overflow_entries`. The proof mirrors that directly: the invariant's
  `overflow_entries = Σ ovfLoadOf` equation is discharged by decomposing the sum at
  the old extent plus identifying each new bucket's overflow occupancy with its
  packed image's count (`ovfCountFun_eq_ovfLoadOf`). No partition lemma exists.
- **Axiom hygiene fix.** `srcPred_true` / `newPred_true` originally used
  `simp [srcPred, hd]`, which pulled `Classical.choice` into their axiom footprint
  (and hence into the stage-4 headline via the selection-count lemmas). They are now
  proved by unfolding the `==`, rewriting with the destination fact, and closing with
  `decide_eq_true rfl`; the headline rests on `propext` / `Quot.sound` only.
- **Slot-bound helper.** `packImg_slot_lt` shows any non-empty packed output slot is a
  genuine slot index (`j < SlotsPerBucket`), via the strict prefix-count gap
  `countMatch_true_lt`.

## Stage 5 addendum — epoch fencing / failure atomicity (anchored 2026-08-26 04:44:01 UTC +0000)

Stage 5 verifies the even/odd mutation fence as explicit `MapState` transitions in
`Lhm/Abs/Epoch.lean`: `beginMutationAt` (map.rs L1229-L1243) opens odd,
`MutationGuard::finish` (map.rs L375-L383) closes at observed + 2, and the parity
gate every entry point performs first (insert map.rs L892-L894, remove L973-L975,
maintenance_step L607-L609, clear L757-L759, reset L689-L691; readers via
`read_consistent_hot` L1203-L1219 over `read_hot_with_epoch`, control.rs L96-L116).
Headlines depend only on `propext` / `Quot.sound` (`#print axioms` re-checks this on
every build); several are axiom-free outright.

| Result | Lean theorem | Content |
|---|---|---|
| Guard behavior | `begin_mutation_at_ok`, `begin_mutation_at_fail_id`, `entry_gate_odd_fails`, `entry_gate_even_ok` | a successful open pins quiescence + observation + u64 headroom and differs from the input only in the epoch; every failure branch carries the store untouched |
| Failure atomicity (ADR 0067) | `run_guarded_fail_atomic`, `apply_split_call_fail_atomic` | when a guarded commit or the split pipeline reports an error, the reported store *is* the input — logical bytes, counters, incarnation, and the even epoch included; all fallible steps precede the first write (map.rs L946, L999, L1663-L1664, L716, L776) |
| Quiescence restore | `run_guarded_ok_epoch` | every committed mutation lands on an even epoch advanced by exactly 2, so odd epochs arise only inside a guard window and are blocked everywhere afterwards (`InProgress` at entry points, `RecoveryRequired` at reopen via validate_control L1048-L1050 = `ValidControl` conjunct 8) |
| Realization | `run_guarded_setValue_realizes`, `_placeAt_realizes`, `_clearSlot_realizes`, `_clearedState_realizes`, `_resetState_realizes`, `apply_split_call_ok_realizes` | the guarded protocol reproduces the stage-3/4 committed states exactly (`splitState` inherits the epoch, so its realization writes the completed epoch on top of it) |
| Write-before-publish (finding 4) | `write_before_publish`, `cleared_published_empty`, `zero_initial_blocks_keeps_stale`, `published_view_inside/outside` | under the newly published extent every readable slot lies either in a rewritten complete block image or coincides with the previous published surface; after clear/reset the published surface is empty despite stale physical bytes beyond the initial extent — grounding stage 3's `fun _ _ => none` abstraction |
| Progress measure (finding 5) | `remSplits`, `next_geometry_rem_splits`, `rem_splits_zero_fails`, `rem_splits_by_buckets`, `geom_chain_bounded`, `retry_loop_terminates` | remaining successful splits from `(level, cursor)` is exactly `2^63 − (2^level + cursor)` (= `2^63 − physical_buckets` under the control equation); each maintenance split consumes one unit, any retry-loop chain is bounded by the initial budget, and at exhaustion `nextGeometry` fails closed |

Modeling decisions recorded during stage 5:

- **Store-carrying failures.** `CallResult.fail err state` returns the store together
  with the error kind, making failure atomicity a contentful equation: an unfaithful
  transcription could return a half-written store on an error path and the theorems
  would catch it. The Rust paths satisfy it because every fallible step precedes the
  first write.
- **Split realization carries the epoch.** Stage 4's `splitState` models the published
  control only and inherits the mutation epoch; the realized post-split state is
  therefore `{ splitState st g with mutationEpoch := st.mutationEpoch + 2 }`,
  mirroring apply_split's order: block images (L1665-L1667), then control
  (L1668-L1675), then the fence closes (L1676). The stage-3 transformers already bake
  in the net "+2", so they realize verbatim.
- **Byte layer split.** `RawStore` (physical, unclamped — stale history survives
  there by design, map.rs L721-L724 / L781-L784 zero only INITIAL_BUCKETS blocks) vs
  `publishedView` (clamped to the published extent, what `scan_physical_window`
  L1088-L1119 and `entries_from_image` L1787-L1806 enumerate). Stages 3–4 model only
  `publishedView`; `write_before_publish` shows why that abstraction is sound.
  The coverage hypothesis — every newly exposed bucket is rewritten before
  publication — is exactly apply_split's two-phase commit (L1665-L1675).
- **Axiom hygiene fix.** An early draft proved the ite lemmas with
  `exact if_pos (by omega)`: elaborating the tactic proof before the condition
  metavariable resolved made Lean fall back to `Classical.propDecidable`, pulling
  `Classical.choice` into `published_view_inside` and everything downstream. Supplying
  the condition explicitly (`if_pos ⟨hb, hj⟩`, lambda proofs for negations) removed
  the dependency; every stage-5 headline now rests on `propext` / `Quot.sound` only.
- **u64 ceiling.** `begin_mutation_at`'s `checked_add(2)` overflow
  (`EpochExhausted`, map.rs L1237) is modeled as the explicit bound
  `mutationEpoch + 2 ≤ U64Max` per SCOPE.md A3.

## Assumption list (see SCOPE.md for full statements)

- A1 hash opacity — rapidhash is uninterpreted; P1–P5 hold for arbitrary hashes. No
  collision-resistance assumption was introduced.
- A2 sequential execution — IC canister message model.
- A3 arithmetic domain — naturals constrained to the u64 ranges the code enforces;
  Rust checked-overflow errors are modeled as explicit fail-closed variants
  (documented on `nextGeometry` / `splitThreshold`). Divergence direction matches:
  both fail closed near the u64 ceiling.
- A4 byte-layer modeling deferred to stage 3+; no byte-layout claims are made here.

No new axioms were introduced. All headline theorems depend only on Lean's standard
`propext` / `Quot.sound`; none depend on `sorryAx` or `Classical.choice`.

## `sorry` list

None. Stage A, stage 3, stage 4, and stage 5 proofs are complete.

## Findings

Severity scale: Critical / High / Medium / Low / Info.

1. **[Low] Duplicated level/cursor derivation — three sites must stay in sync. Remediated.**
   At the audited revision, `control.rs` `decode`, `control.rs` `read_hot_with_epoch`,
   and `map.rs` `scrub_control` each independently re-derive
   `(level, split_cursor)` from `physical_buckets`. They agree today (the Lean model's
   single derivation is exactly what all three compute), but this is a
   single-source-of-truth gap inside the crate: a future edit to one site can silently
   desynchronize routing. Remediated on 2026-08-24 (21:31 UTC): all three sites now call
   one `fn derive_geometry(physical_buckets) -> (u8, u64)` at control.rs L48-L57;
   current anchors: `decode` control.rs L59-L73, `read_hot_with_epoch` control.rs
   L96-L116, `scrub_control` map.rs L1153-L1166.
2. **[Info] `read_hot_with_epoch` lacks the zero guard its sibling has. Remediated.**
   At the audited revision, `read_hot_with_epoch` computed `level = 63 -
   leading_zeros(pb)` and subtracted `1 << level` without checking; only `decode`
   special-cased `pb == 0`. With `pb == 0` the subtraction `63 - 64` underflows
   (debug panic; release wrap would yield a garbage level into a shift). Unreachable
   through public APIs because `open` → `validate_control` rejects
   `physical_buckets < INITIAL_BUCKETS = 8`, and the handle cannot observe `pb == 0`
   afterwards. Remediated on 2026-08-24 (21:31 UTC) together with finding 1:
   `derive_geometry` guards `pb == 0` once and degrades it to the empty geometry
   `(0, 0)` without trapping or wrapping; `read_hot_with_epoch` (control.rs L96-L116)
   routes through it. A unit test pins the contract, including the degraded input.
3. **[Info] Shift-safety obligation for `linear_bucket` is satisfied but implicit.**
   `hash & ((mask << 1) | 1)` requires `level < 64`. The chain that guarantees it
   (open validates `level < 63`; `next_geometry` caps increments below 63; hot-path
   levels are re-derived from validated `pb < 2^63`) is correct but spread across
   three files. The Lean side now states the obligation explicitly on `linearBucket`.
4. **[Info] `reset`/`clear` leave stale bytes beyond the initial extent.**
   map.rs L706-L724 / L766-L784 zero only the first `INITIAL_BUCKETS` blocks. Bytes
   written by earlier growth remain until reused. Safe today because a later
   `apply_split` writes complete block images before publishing the larger geometry
   (map.rs L1662-L1677), so stale bytes are never readable under a published control.
   Formalized in stage 5: the write-before-publish ordering is encoded as
   `write_before_publish` in `Lhm/Abs/Epoch.lean`, with `zero_initial_blocks_keeps_stale`
   capturing the premise and `cleared_published_empty` proving the published surface
   empty after clear despite surviving physical bytes.
5. **[Info] `insert` retry-loop termination is informal.**
   map.rs L886-L964: after a maintenance split the loop re-reads control and retries.
   Termination relies on `physical_buckets` increasing monotonically toward the
   geometry cap where `next_geometry` fails closed. True, but worth a stage-5 lemma
   (progress measure) rather than prose.
   Formalized in stage 5: `remSplits level cursor = 2^63 − (2^level + cursor)` in
   `Lhm/Abs/Epoch.lean` strictly decreases per successful split
   (`next_geometry_rem_splits`), bounds every retry-loop chain
   (`geom_chain_bounded`, so `retry_loop_terminates`), and at zero the geometry step
   fails closed (`rem_splits_zero_fails`).

## Conclusion

Stage A verifies cleanly: the linear-hashing routing mathematics and the
control-region contract enforced at open are internally consistent, and the routing
extent guarantee holds for arbitrary hash values under the documented invariant. The
audit produced no correctness defect in the verified scope; findings 1–2 are cheap
hygiene improvements (both applied post-audit, see above), and findings 3–5 define
the proof obligations that later stages must formalize.

The stage-3 addendum extends this to the abstract logical map: insert (update, place,
refusal), remove, clear, and reset all preserve `Inv`, with the two-choice search
shown complete under it. The stage-4 addendum closes split preservation: a successful
maintenance split relocates entries only to `source` or `source + base(level)`,
re-packs both destination blocks without loss, and keeps `len` / `overflow_entries`
consistent with per-bucket occupancy — so `Inv` survives every split the control
layer can publish.

The stage-5 addendum closes the remaining obligations: the mutation fence is modeled
as explicit state transitions whose failures provably carry the store untouched (ADR
0067's prewrite atomicity), committed operations realize the stage-3/4 states exactly,
an interrupted window fences every entry point and reopen until recovery,
`apply_split`'s write-before-publish ordering makes stale bytes beyond the published
extent permanently unreadable (finding 4), and the insert retry loop terminates by an
explicit progress measure that fails closed at the geometry cap (finding 5). All
stages of the SCOPE.md roadmap are now verified with no `sorry`.
