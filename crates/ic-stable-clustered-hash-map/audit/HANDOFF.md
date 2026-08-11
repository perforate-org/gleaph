# Resolved historical provenance: `insert_preserves_invariant`

Last updated: 2026-08-10
Anchor timestamp: 2026-08-11 05:37:59 UTC +0000

This file records the historical proof handoff; it is not current implementation guidance.

## Historical baseline

- `9b768b096` remains the historical clean/build-green baseline for this audit handoff.
- The current theorem uses the indexed `InsertRelocateOK h` certificate directly. The old
  `OkRelates` and certificate re-encoding experiments are retired historical attempts, not
  follow-up advice and not part of the current Lean tree.

## Resolved proof and current boundary

`insert_preserves_invariant` is now proved by induction over the supplied,
already-certified `InsertRelocateOK` chain. The result is conditional on
`remapEnd = none`; it does not prove Rust constructs the certificate, active-remap
insertion, or a relocation chain that enters `size_up` mid-chain.

`sizeUp_preserves_entries` proves the `SizeUp` relation. By contrast, `remap_preserves_entries` is relation-level because `RemapStep` postulates `keySet` and `len`; it is not a Rust refinement proof of production `remap_step` or `remap_position`. Production `remap_position` may re-expand `remap_end`, and
`ExpectedBucket` is not yet proved a faithful invariant for active remapping.

The machine-checked `remapStep_does_not_preserve_clusterInvariant` counterexample shows
that the current weak `RemapStep` relation admits an invariant-preserving source and an
invariant-breaking target. Consequently, the named theorem
`remap_step_preserves_invariant` is false as currently stated, not merely deferred.

The machine-checked `unrelocateStep_does_not_preserve_clusterInvariant` counterexample
is parameterized by a supplied `k : Key` and therefore requires an inhabited key domain.
For each such key, the current weak `UnRelocateStep` relation admits a valid source and an
invariant-breaking target. Consequently, `remove_preserves_invariant` is false for an
inhabited key domain, not merely deferred. This exposes a Lean relation defect, not a Rust
implementation defect or evidence that the modeled counterexample is Rust-reachable.

`UnRelocateStepWithStableHeader` now wraps the retained weak relation with
`SameRemoveHeader`, and the compiler-checked
`unrelocateStepWithStableHeader_preserves_inBounds` theorem proves source and target have
the same in-bounds slots. This closes only the counterexample's header/geometry route.
The helper itself does not model faithful continue/stop/chain behavior, does not close
`remove_preserves_invariant`, and does not alter the two remaining `sorry`s.

A separate bounded model now records the faithful inner `remove_and_relocate` execution:
`RemoveFrame` restricts each write to the current hole, `RemoveContinue` copies the exact
tail while leaving its old slot stale, `ClearCurrentHole` / `RemoveStop` model the terminal
clear and guards, and inductive `RemoveRelocate` threads the chain. The compiler-checked
`RemoveContinue.oldTailUnchanged` and `RemoveRelocate.sameHeader` lemmas establish the
named local facts. `clearCurrentHole_preserves_clusterInvariant` and
`removeContinue_preserves_clusterInvariant` establish the stop/continue cases, and the
kernel-checked `removeRelocate_preserves_invariant` theorem proves the full faithful chain
preserves `ClusterInvariant` under `s.remapEnd = none`. The machine-checked
`removeRelocate_activeBoundary_counterexample` shows why that settled premise cannot be
dropped under the current `ExpectedBucket` invariant. The existing admitted removal
theorem still targets the weak `UnRelocateStep` relation.

`PublicRemoveSettled` and `publicRemoveSettled_preserves_invariant` now provide the
smallest public-operation bridge for the settled found branch. The certificate records the
modeled lookup-selected position, `s.remapEnd = none`, the faithful `RemoveRelocate`
chain, and the final `{ mid with len := mid.len - 1 }` update. The theorem proves the
invariant, `0 < s.len`, `len - 1`, and `n`/`remapEnd` header preservation. It consumes
these certificates; it does not prove that the leading Rust `remap_step` or concrete
lookup constructs them, and it does not cover active-remap, absent-key, or persistence /
re-open refinement.

The settled lookup success direction is now closed by
`lookupIndex_some_implies_lookupFound`: under `remapEnd = none`, a modeled successful
scan returns an in-bounds occupied slot containing the requested key at its expected
bucket. This is only a soundness direction; lookup completeness and the Rust construction
of the `PublicRemoveSettled` certificate remain open.
`publicRemoveSettled_lookupFound` exposes the same result directly from the public-remove
certificate without strengthening the settled invariant assumptions.

`lookupIndex_completeness_counterexample` now machine-checks the remaining model gap: an
otherwise invariant settled state may place an occupied entry after an empty slot, so the
modeled scan returns `none` even though the key is in `KeySet`. The next completeness proof
must add a no-holes / scan-contiguity condition or an equivalent insertion-history relation;
this counterexample is not a Rust reachability claim.

The explicit `NoHoles` strengthening and
`lookupIndex_complete_of_noHoles` now close the settled lookup completeness direction
when `ClusterInvariant`, `NoHoles`, `LenCoherent`, and `remapEnd = none` are supplied.
`len_pos_of_lenCoherent_keySet` derives `0 < s.len` from a key in `KeySet`. The proof does
not establish `NoHoles` or `LenCoherent` from Rust insertion history; those are the next
refinement obligations.

`clusterInvariant_does_not_imply_len_positive` machine-checks the length side separately:
the same invariant and `KeySet` witness remains valid after changing only `len` to zero.
Any end-to-end completeness proof therefore needs both the no-holes condition and a
`LenCoherent` cardinality/length relation.

`InsertRelocateOccupancyOK` records the explicit in-bounds and non-EMPTY slot facts needed
for a cardinality proof separate from `InsertRelocateOK`. The kernel-checked
`insertRelocate_preserves_occupiedCard` theorem shows that a certified chain adds exactly
one occupied slot, and `publicInsertSettled_preserves_lenCoherent` carries that result
through the final `len + 1` update when the chain header length is supplied. These
certificate premises are not yet derived from Rust insertion history.

`relocateWrite_preserves_noHoles` now proves the terminating insert write preserves
`NoHoles` when every slot from the new key's bucket to the insertion position is occupied.
That prefix premise is intentionally explicit because neither `RelocateWrite` nor
`InsertRelocateOK` records the scan's stop-at-empty fact. The compiler-checked
`findInsertPositionFrom_prefix` and `findInsertPosition_prefix` lemmas now extract the
occupied prefix traversed by the initial Rust `find_insert_position` scan. Per-step
`relocateStep_next_prefix_of_noHoles` derives the next pending entry's occupied prefix
from `NoHoles` plus the end-of-cluster scan when the current position and pending distance
are bounded. `insertRelocateNoHolesOK_of_occupancyOK` and
`insertRelocate_preserves_noHoles_of_occupancyOK` now thread that conditional prefix
through a complete chain when `InsertRelocateOccupancyOK` is supplied. Deriving the
occupancy/bound certificates from Rust remains open.

`relocateStep_preserves_noHoles` now covers the intermediate write under the same prefix
and non-EMPTY pending-distance premises. The displaced entry is pending rather than present
in that intermediate state, so the theorem is deliberately local; the inductive chain
composition is closed by the certificate below, while per-step Rust derivation remains open.

`InsertRelocateNoHolesOK` and `insertRelocate_preserves_noHoles` now compose the terminating
and relocation-step cases across the full certified chain. The per-step occupied prefixes
and non-EMPTY distances remain explicit; this is a chain-level conditional theorem, not yet
a derivation of every displaced-entry prefix from Rust's `find_insert_position` execution.
The initial scan prefix is now extracted by `findInsertPositionFrom_prefix` and
`findInsertPosition_prefix`, and `relocateStep_next_prefix_of_noHoles` supplies the
conditional next-step prefix. The Rust distance/bound derivation and certificate
construction remain open, while the conditional propagation is closed by
`insertRelocateNoHolesOK_of_occupancyOK`.
`insertRelocate_preserves_noHoles_of_findPosition` now supplies the initial prefix from an
explicit `findInsertPosition` result and closes the complete-chain `NoHoles` bridge when
the source `NoHoles` and occupancy certificate are supplied.

The abstract `freshState` now models the cleared table produced by Rust `new`. The
compiler-checked `freshState_clusterInvariant`, `freshState_noHoles`,
`freshState_lenCoherent`, and `freshState_keySet_empty` lemmas close the initial base state;
they do not refine persisted header validation or `init`.

The independent P1 / High `size_up` allocation defect recorded in `GAP-2026-08-10-002`
is repaired in commit `c1dc31db7`: `size_up` derives its growth target from
the canonical `entry_stride()`, and a focused normal-load-threshold regression covers the
previous out-of-bounds write. This runtime evidence does not extend the Lean result;
the proof still does not establish production resize safety.

`reopen_consistent_of_cluster_invariant` is now kernel-checked, but only at the abstract
predicate level: it proves `KeySet` equivalence with the existential `LookupFound`
predicate through `EntryAtCorrectBucket`. It does not refine the actual `lookupIndex`, a
persisted memory image, or `init` / re-open behavior.

## Follow-on proofs

1. Derive source `NoHoles` / `LenCoherent` after arbitrary insertion history, plus the per-step
   position/distance/occupancy certificate premises from Rust insertion history. The initial scan prefix is extracted by
   `findInsertPositionFrom_prefix` / `findInsertPosition_prefix`, and
   `relocateStep_next_prefix_of_noHoles` plus
   `insertRelocateNoHolesOK_of_occupancyOK` now supply conditional per-step propagation;
   `insertRelocate_preserves_noHoles_of_findPosition` closes the scan-to-chain bridge; next
   connect the remaining certificates to the per-step end-of-cluster history before refining the
   leading `remap_step` into the `PublicRemoveSettled` certificate and cover the absent-key
   and active-remap public branches before retiring the weak
   `UnRelocateStep`-targeted `remove_preserves_invariant` obligation.
2. Strengthen `RemapStep` with the slot and invariant facts guaranteed by incremental
   remapping, then restate and prove `remap_step_preserves_invariant`.
