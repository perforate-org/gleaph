# Verification Report

Date (UTC): 2026-08-09
Last updated: 2026-08-10
Anchor timestamp: 2026-08-10 08:52:05 UTC +0000

## Target and version / Mode

- **Target**: `ic-stable-clustered-hash-map` — `StableClusteredHashMap` (src/map.rs), a
  stable-memory clustered (Amble & Knuth ordered) hash table with incremental in-place
  resize, at commit `9b768b096`.
- **Mode**: **audit**. An existing implementation was transcribed into Lean 4 (Mathlib
  v4.32.2) and its invariants proved / counterexamined.

## Scope (Stage 0)

- **Targets**:
  - (a) entry preservation across resize (`ResizePreservesEntries`),
  - (b) cluster invariant preserved by insert/remove/remap (`ClusterInvariant`),
  - (c) re-open mid-resize consistency (`ReopenConsistent`).
- **Out of scope**: byte-level layout / header magic, memory growth and `OutOfMemory`
  /`InsertError` paths, `Storable` serialization internals, iteration order, benchmarks.

## Method

The `lean-formal-audit` workflow, in `audit/StableClusterAudit/` (a Lake project with
Mathlib):

- Stage 1 — `Abstract.lean`: state model, cluster invariants, target properties,
  assumptions.
- Stage 1 adversarial — `Counterexamples.lean`: counterexamples to a claimed bound and
  to invariant preservation by the current `RemapStep` relation.
- Stage 2 — `Map.lean`: relation-level models of scanning, probing, insert/remove
  relocation, and incremental resize, with their stated assumptions.
- Stage 3 — `Soundness.lean`: proofs of the target properties.
- Stage 4 — this report.

The direct command `lake env lean StableClusterAudit/Soundness.lean` succeeds and its output
contains exactly the two preserved `sorry` warnings for remove and remap.

## Assumption list

Documented in `Abstract.lean` (SCOPE §5), not axiomatized (stated as `True` has no
content); added as axioms only if a proof required one:

- Single-threaded (canister); abstract memory get/set is correct; stable memory is not
  corrupted.
- `hash` (rapidhash v3, constant seed) is deterministic and treated as a black-box
  function; its internals and collision resistance are not verified.
- Distances fit in `u32`; this is enforced at insert by `checked_distance` (traps on
  overflow), not guaranteed by the structure alone.
- Callers honor the documented `&self` aliasing rule.

## Findings per file

### `Abstract.lean` (Stage 1)

- **`DistanceBounded` (B4, "distances are bounded by the overflow area") is NOT a
  structural invariant.** Removed from `ClusterInvariant`; see `Counterexamples.lean`.

### `Counterexamples.lean` (Stage 1 adversarial)

- **B4 counterexample (proved in Lean)**: a valid clustered table can hold an entry
  whose distance exceeds the overflow-area size `n`. E.g. `n = 3`, a bucket-0 cluster of
  5 entries has max distance `4 > 3`. Consequence: with the old `u16` storage, an
  adversarial or unlucky distribution could overflow `u16`, colliding with the `EMPTY`
  marker and silently corrupting the table.
- **`RemapStep` counterexample (proved in Lean)**:
  `remapStep_does_not_preserve_clusterInvariant` constructs `remapGoodState` and
  `remapBadState`, which have the same empty `KeySet`, zero `len`, and compatible
  `remapEnd`, while only the source satisfies `ClusterInvariant`. Therefore the named
  theorem `remap_step_preserves_invariant` is false under the current weak `RemapStep`
  relation; it is not merely a deferred proof.

### `Map.lean` (Stage 2 transcription — several under-specifications surfaced)

- `RelocateWrite` set the written entry's distance to `0`; its relation now specifies
  `position - bucket`.
- `SizeUp` did not state the newly grown region is cleared (`clear_region`); without
  that, entry preservation is unprovable. Fixed (structure).
- `RemapStep` postulates `keySet` and `len` but leaves slot contents unconstrained;
  `remap_preserves_entries` is therefore relation-level, not a Rust refinement proof of
  production `remap_step` or `remap_position`, and the current relation admits the
  invariant-breaking counterexample above.
- `RelocateStep` modeled a complete move (writing the displaced entry at `next`); it is
  in fact an **intermediate state** with the displaced entry in flight, and its distance
  handling was wrong. Refactored into a Type-valued relation with `entryDist` and a
  `remapEnd`-preservation field.
- The `insert` loop is formalized as an `InsertRelocate` chain (relocation steps ended by
  a `RelocateWrite`) carrying per-step well-formedness in an inductive `InsertRelocateOK`
  (order boundary, home bucket, precedes-the-displaced property).

### `Soundness.lean` (Stage 3)

Proved:

- Target (a): `sizeUp_preserves_entries` is proved for `SizeUp`; `remap_preserves_entries` is proved only for `RemapStep`, which postulates `keySet` and `len`, not a Rust refinement proof of production `remap_step` or `remap_position`.
- Target (b), step level: `relocateWrite_preserves_clusterInvariant` (base write) and
  `relocateStep_preserves_clusterInvariant` (single relocation step) — **proved**.
- The chain maintenance machinery — `displaced_home_bucket`, `bucketAt_in_scan`,
  `endOfClusterFrom_ge`/`_le_capacity`, `order_boundary_of_cluster_end`, and
  `relocateStep_preserves_order_boundary` (the boundary survives a relocation step) —
  **all proved**.
- `insert_preserves_invariant` over the `InsertRelocateOK` chain is proved conditionally:
  a supplied, already-certified settled chain preserves `ClusterInvariant` under
  `remapEnd = none`. It does not prove that Rust constructs `InsertRelocateOK`, insertion
  during active remapping, or a relocation chain that enters `size_up` mid-chain.
- Target (c), predicate level: `reopen_consistent_of_cluster_invariant` is kernel-checked,
  but proves only `KeySet` equivalence with the existential `LookupFound` predicate through
  `EntryAtCorrectBucket`; it does not refine the actual `lookupIndex`, a persisted memory
  image, or `init` / re-open behavior.

Not proved, and false as currently stated:

- Target (b), remap: `remap_step_preserves_invariant` is false under the current weak
  `RemapStep` relation. The machine-checked
  `remapStep_does_not_preserve_clusterInvariant` counterexample shows that preserving
  `KeySet`, `len`, and the remap boundary does not constrain slot contents enough to
  preserve `ClusterInvariant`. The relation must be strengthened before this theorem can
  be proved.

The `RemapStep` result is relation-level because it postulates `keySet` and `len`; it is not a Rust refinement proof. Production `remap_position` can
re-expand `remap_end`, and `ExpectedBucket` has not been proved a faithful invariant while
a remap is active. The independent P1 / High `size_up` allocation defect recorded in
`GAP-2026-08-10-002` is repaired in the current uncommitted worktree by deriving growth from
the canonical `entry_stride()`; a focused normal-load-threshold regression covers the prior
out-of-bounds write. This runtime evidence does not extend the Lean relations, and no production
resize assurance is inferred from those relations.

## Findings (severity)

- **High (now fixed in the current uncommitted worktree)**: `size_up` used a stale
  `key_size + value_size + 2` growth stride while entry offsets and clearing used the canonical
  `key_size + value_size + 4` stride. It now calls `entry_stride()`, and
  `load_threshold_resize_allocates_the_canonical_entry_stride` exercises the previously failing
  minimal-page `n = 13 → 14` normal threshold path.
- **Medium (now fixed)**: u16 distance overflow could silently corrupt the persisted
  table when a single bucket's cluster exceeded 65535 entries (adversarial keys or very
  large, unluckily-clustered maps). **Resolution implemented in Rust**: distances are
  now `u32` and fit is enforced at insert by `checked_distance`, which traps on overflow
  — on the IC a trap rolls back the whole message, so no partial corruption.
- **Info**: the code comment "distances are bounded by the overflow area N" was
  inaccurate (non-structural); the audit's Lean counterexample documents why.
- **Info**: several Stage 2 relations were under-specified. In particular, the current
  weak `RemapStep` relation makes `remap_step_preserves_invariant` false, as demonstrated
  by the machine-checked counterexample. The other relations' current forms record the
  local facts required by the corresponding proofs; they do not establish generic
  implementation fidelity.

## List of `sorry` / unproven spots and interpretation

| Target     | Theorem                                    | Why unproved / what is needed                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| ---------- | ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| (b) remove | `remove_preserves_invariant`               | Same relocation-chain maintenance for `remove_and_relocate`'s gap-fill.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| (b) remap  | `remap_step_preserves_invariant`           | False under the current weak `RemapStep` relation, not merely deferred: `remapStep_does_not_preserve_clusterInvariant` supplies a machine-checked counterexample. Strengthen the relation with the slot and invariant facts guaranteed by remapping before attempting the proof. `remap_preserves_entries` remains relation-level because `RemapStep` postulates `keySet` and `len`, not a Rust refinement proof of production remapping. |

These two theorem bodies remain admitted. The conditional settled insert-chain theorem
is closed; remove invariant preservation remains unproved, while remap invariant
preservation is false under the current relation and first requires a stronger model.

## Conclusion

The audit recorded a historical `u16` distance-overflow bug that was subsequently fixed
in Rust with `u32` storage and insertion-time trapping. In the current Lean relations it
proves `SizeUp` entry preservation, the relation-level `RemapStep` entry result (it postulates `keySet` and `len`, so it is not a Rust refinement proof), base-write and
single-relocation-step invariant preservation, chain-maintenance lemmas, and the
conditional settled insert-chain theorem. The remaining open items are the remove chain
argument and strengthening `RemapStep` before proving remap invariant preservation; the
current theorem is refuted by a machine-checked counterexample. The target (c) theorem is
closed only at the abstract predicate level; actual lookup and persistence/re-open
refinement remain outside that proof.

## Status against "formal proof of a trustworthy data structure"

This is **not yet a complete** operation-level verification. `insert_preserves_invariant`
is conditional on a certified `InsertRelocateOK` settled chain and `remapEnd = none`; it
does not cover Rust certificate construction, active-remap insertion, or mid-chain
`size_up`. Remove and remap invariant preservation remain the two `sorry`s, but they have
different status: remove is unproved, whereas `remap_step_preserves_invariant` is false
under the current weak `RemapStep` relation. The proved target (c) predicate equivalence
does not cover the actual `lookupIndex`, persisted images, or `init` / re-open refinement,
so the structure should not be presented as formally verified end-to-end.

The model exposes its local assumptions explicitly: `SizeUp` records a cleared grown
region, `RemapStep` postulates `keySet` and `len`, leaves slot contents too weakly
constrained for invariant preservation, and is not a Rust refinement proof;
`RelocateWrite` specifies its distance, and `RelocateStep` has in-flight semantics. These
statements describe named relations, not an end-to-end or generic Rust-fidelity claim.
