# Resolved historical provenance: `insert_preserves_invariant`

Last updated: 2026-08-10
Anchor timestamp: 2026-08-10 06:53:00 UTC +0000

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

The independent P1 / High `size_up` allocation defect recorded in `GAP-2026-08-10-002`
is repaired in the current uncommitted worktree: `size_up` derives its growth target from
the canonical `entry_stride()`, and a focused normal-load-threshold regression covers the
previous out-of-bounds write. This runtime evidence does not extend the Lean result;
the proof still does not establish production resize safety.

## Follow-on proofs

1. `remove_preserves_invariant` for the `UnRelocateStep` gap-fill chain.
2. `remap_step_preserves_invariant` for incremental remapping.
3. `reopen_consistent_of_cluster_invariant` and the lookup / `KeySet` correspondence.
