# Resolved historical provenance: `insert_preserves_invariant`

Last updated: 2026-08-10
Anchor timestamp: 2026-08-10 14:09:56 UTC +0000

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
named local facts. The machine-checked `removeRelocate_activeBoundary_counterexample`
blocks faithful preservation while `remapEnd` is active under its stated bucket premises;
no no-remap preservation theorem is closed yet. The existing admitted removal theorem still
targets `UnRelocateStep`.

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

1. Restate removal invariant preservation over the new `RemoveRelocate` chain, prove the
   required per-step and chain invariants, and only then retire the weak
   `UnRelocateStep`-targeted `remove_preserves_invariant` obligation.
2. Strengthen `RemapStep` with the slot and invariant facts guaranteed by incremental
   remapping, then restate and prove `remap_step_preserves_invariant`.
