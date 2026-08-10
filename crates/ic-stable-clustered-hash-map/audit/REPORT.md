# Verification Report

Date (UTC): 2026-08-09
Last updated: 2026-08-10
Anchor timestamp: 2026-08-10 23:42:29 UTC +0000

## Target and version / Mode

- **Target**: `ic-stable-clustered-hash-map` — `StableClusteredHashMap` (src/map.rs), a
  stable-memory clustered (Amble & Knuth ordered) hash table with incremental in-place
  resize; the current Rust source under audit is commit `c1dc31db7`.
- **Provenance**: `9b768b096` is the initial clean/build-green Lean baseline, while
  `a50670417` is the audit-artifact commit for the faithful remove-chain model.
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
  to invariant preservation by the current `UnRelocateStep` and `RemapStep` relations,
  plus lookup completeness under the current `ClusterInvariant`.
- Stage 2 — `Map.lean`: relation-level models of scanning, probing, insert/remove
  relocation, and incremental resize, with their stated assumptions.
- Stage 3 — `Soundness.lean`: proofs of the target properties.
- Stage 4 — this report.

The direct command `lake env lean StableClusterAudit/Soundness.lean` succeeds and its output
contains exactly the two preserved `sorry` warnings for the weak `UnRelocateStep` and
`RemapStep` invariant-preservation declarations.

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
- **`UnRelocateStep` counterexample (proved in Lean)**:
  `unrelocateStep_does_not_preserve_clusterInvariant` constructs, for every supplied
  `k : Key`, a valid source and invalid target. The weak relation permits the target table
  geometry to change and leaves an invalid in-bounds distance at slot 3. Thus, for an
  inhabited key domain, it refutes `remove_preserves_invariant` under the current relation,
  not merely as a deferred proof. This exposes a Lean relation defect, not a Rust
  implementation defect or evidence that the modeled counterexample is Rust-reachable.
  `UnRelocateStepWithStableHeader` and
  `unrelocateStepWithStableHeader_preserves_inBounds` close only this header/geometry
  route by preserving which slots are in bounds. The helper itself does not model the
  faithful continue/stop/chain behavior, does not close `remove_preserves_invariant`, and
  does not alter the two remaining `sorry`s.
- **Lookup completeness counterexample (proved in Lean)**:
  `lookupIndex_completeness_counterexample` constructs a settled state with a valid
  `ClusterInvariant` and a key in `KeySet`, but an empty slot between the key's home bucket
  and its occupied slot. The modeled scan therefore returns `none`. This is a Lean-model
  structural counterexample, not evidence that Rust insertion reaches the state; a
  no-holes / scan-contiguity condition or insertion-history relation is required before
  proving lookup completeness.

### `Map.lean` (Stage 2 transcription — several under-specifications surfaced)

- `RelocateWrite` set the written entry's distance to `0`; its relation now specifies
  `position - bucket`.
- `SizeUp` did not state the newly grown region is cleared (`clear_region`); without
  that, entry preservation is unprovable. Fixed (structure).
- `RemapStep` postulates `keySet` and `len` but leaves slot contents unconstrained;
  `remap_preserves_entries` is therefore relation-level, not a Rust refinement proof of
  production `remap_step` or `remap_position`, and the current relation admits the
  invariant-breaking counterexample above.
- `UnRelocateStep` constrains only the moved tail entry and slots other than `position`
  and `next`; it does not preserve table geometry or constrain all target slots, so it
  admits the invariant-breaking counterexample above for any inhabited key domain.
- A separate bounded model now transcribes the inner `remove_and_relocate` loop:
  `SameRemoveHeader` fixes `n`, `len`, and `remapEnd`; `RemoveFrame` limits each write to
  the current hole; `RemoveContinue` copies the exact cluster tail while leaving its old
  slot stale; `ClearCurrentHole` / `RemoveStop` model the terminal clear and guards; and
  inductive `RemoveRelocate` composes the continue/stop chain. Compiler-checked lemmas
  `RemoveContinue.oldTailUnchanged` and `RemoveRelocate.sameHeader` establish the stated
  stale-tail and header facts. `removeRelocate_preserves_invariant` proves this faithful
  chain preserves `ClusterInvariant` when the source is settled (`s.remapEnd = none`).
  `removeRelocate_activeBoundary_counterexample` remains machine-checked and shows why
  that settled premise cannot be dropped under the current `ExpectedBucket` invariant.
- `PublicRemoveSettled` and `publicRemoveSettled_preserves_invariant` connect that faithful
  chain to a certificate-level, settled found branch of public `remove`. The theorem proves
  `RemovePreservesInvariant`, a positive source length, the final `len - 1`, and preservation
  of `n` and `remapEnd`. It consumes the modeled `lookupIndex` result and relocation
  certificate; it does not prove that Rust's leading `remap_step` or concrete lookup
  constructs them, and it does not cover active remapping, an absent key, or persistence.
- `UnRelocateStepWithStableHeader` is a narrower helper around the retained weak
  `UnRelocateStep`; it uses `SameRemoveHeader` but is not the faithful chain model.
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
- `unrelocateStepWithStableHeader_preserves_inBounds` proves only that the bounded
  stable-header helper cannot make a target-only slot in bounds. It closes the specific
  header/geometry counterexample route, not the slot and chain obligations needed for
  the retained weak `UnRelocateStep` declaration.
- `clearCurrentHole_preserves_clusterInvariant` proves that the terminal clear preserves
  the invariant; `removeContinue_preserves_clusterInvariant` proves each faithful copy
  step preserves it under `s.remapEnd = none`; and
  `removeRelocate_preserves_invariant` composes those facts across the full bounded
  `RemoveRelocate` chain. This is a settled inner-loop theorem, not active-remap or
  end-to-end public `remove` refinement.
- `publicRemoveSettled_preserves_invariant` proves the settled found-branch certificate
  preserves the invariant through the final public `len - 1` update and preserves the
  state header. It consumes, rather than constructs, the lookup and relocation
  certificates; leading `remap_step`, concrete lookup refinement, active-remap removal,
  absent-key behavior, and persistence remain outside the theorem.
- `lookupIndex_some_implies_lookupFound` proves the settled concrete scan-result direction:
  a modeled `lookupIndex` success yields an in-bounds occupied slot containing the requested
  key at its expected bucket. It does not prove completeness, so it does not yet show that
  every stored key is found or that Rust's full lookup path constructs the remove certificate.
  `publicRemoveSettled_lookupFound` forwards that result through the public-remove
  certificate without adding a stronger invariant assumption.
- The current `ClusterInvariant` does not imply lookup completeness: the machine-checked
  `lookupIndex_completeness_counterexample` places an occupied entry after an empty slot
  in an otherwise invariant state. The scan's stop-at-empty behavior is therefore not
  derivable from the present invariant alone.
- `NoHoles` is now an explicit strengthening, and
  `lookupIndex_complete_of_noHoles` proves settled lookup completeness from
  `ClusterInvariant`, `NoHoles`, and `LenCoherent`. The theorem preserves the boundary
  honestly: the Rust/insertion-history proof of `NoHoles` and `LenCoherent` remains
  unproved.
- `clusterInvariant_does_not_imply_len_positive` machine-checks the second boundary:
  changing only `len` to zero preserves the current `ClusterInvariant` and `KeySet`.
  `LenCoherent` supplies that separate cardinality relation and derives positive length
  for a key in `KeySet`.
- `InsertRelocateOccupancyOK` and `insertRelocate_preserves_occupiedCard` prove that a
  certified insert chain changes occupied-slot cardinality by exactly one: relocation
  steps overwrite occupied slots and the terminating write fills one in-bounds empty slot.
  `publicInsertSettled_preserves_lenCoherent` then proves the public `len + 1` update
  preserves `LenCoherent` when the chain's header length is supplied. These occupancy and
  header premises are not derivable from the existing `InsertRelocateOK` relation or from
  Rust insertion history yet.
- `insert_preserves_invariant` over the `InsertRelocateOK` chain is proved conditionally:
  a supplied, already-certified settled chain preserves `ClusterInvariant` under
  `remapEnd = none`. It does not prove that Rust constructs `InsertRelocateOK`, insertion
  during active remapping, or a relocation chain that enters `size_up` mid-chain.
- Target (c), predicate level: `reopen_consistent_of_cluster_invariant` is kernel-checked,
  but proves only `KeySet` equivalence with the existential `LookupFound` predicate through
  `EntryAtCorrectBucket`; it does not refine the actual `lookupIndex`, a persisted memory
  image, or `init` / re-open behavior.

Remaining admitted weak-relation declarations, false as currently stated:

- Target (b), remove: for any inhabited key domain, `remove_preserves_invariant` is
  refuted under the current weak `UnRelocateStep` relation. The machine-checked
  `unrelocateStep_does_not_preserve_clusterInvariant` counterexample shows that the
  relation permits a valid source and invalid target; it identifies a Lean relation defect,
  not a Rust implementation defect. The faithful `RemoveContinue` / `RemoveStop` /
  `RemoveRelocate` model is now present, but `remove_preserves_invariant` still names the
  retained weak relation and remains admitted. Separately,
  `removeRelocate_preserves_invariant` proves invariant preservation for the faithful
  bounded chain under `s.remapEnd = none`.

- Target (b), remap: `remap_step_preserves_invariant` is false under the current weak
  `RemapStep` relation. The machine-checked
  `remapStep_does_not_preserve_clusterInvariant` counterexample shows that preserving
  `KeySet`, `len`, and the remap boundary does not constrain slot contents enough to
  preserve `ClusterInvariant`. The relation must be strengthened before this theorem can
  be proved.

The `RemapStep` result is relation-level because it postulates `keySet` and `len`; it is not a Rust refinement proof. Production `remap_position` can
re-expand `remap_end`, and `ExpectedBucket` has not been proved a faithful invariant while
a remap is active. The independent P1 / High `size_up` allocation defect recorded in
`GAP-2026-08-10-002` is repaired in commit `c1dc31db7` by deriving growth from
the canonical `entry_stride()`; a focused normal-load-threshold regression covers the prior
out-of-bounds write. This runtime evidence does not extend the Lean relations, and no production
resize assurance is inferred from those relations.

## Findings (severity)

- **High (fixed in commit `c1dc31db7`)**: `size_up` used a stale
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
| (b) remove | `remove_preserves_invariant`               | For any inhabited key domain, false under the retained weak `UnRelocateStep` relation, not merely deferred: `unrelocateStep_does_not_preserve_clusterInvariant` supplies a machine-checked counterexample for each supplied `k : Key`. `UnRelocateStepWithStableHeader` plus `unrelocateStepWithStableHeader_preserves_inBounds` closes only that counterexample's header/geometry route; the helper itself does not model the separately added faithful continue/stop/chain relation. The admitted theorem still targets `UnRelocateStep`; independently, `removeRelocate_preserves_invariant` proves the faithful bounded `RemoveRelocate` chain preserves `ClusterInvariant` under `s.remapEnd = none`. This identifies a Lean relation defect, not a Rust implementation defect. |
| (b) remap  | `remap_step_preserves_invariant`           | False under the current weak `RemapStep` relation, not merely deferred: `remapStep_does_not_preserve_clusterInvariant` supplies a machine-checked counterexample. Strengthen the relation with the slot and invariant facts guaranteed by remapping before attempting the proof. `remap_preserves_entries` remains relation-level because `RemapStep` postulates `keySet` and `len`, not a Rust refinement proof of production remapping. |

These two theorem bodies remain admitted. `remap_step_preserves_invariant` is false under
its current weak relation; `remove_preserves_invariant` has a counterexample for every
supplied `k : Key` and is therefore false for any inhabited key domain. The conditional
settled insert-chain theorem, faithful settled remove-chain theorem, and certificate-level
settled found-branch public-remove bridge are closed. Remap invariant preservation still
requires a stronger model. The two weak-relation admitted theorem bodies are unchanged.

## Conclusion

The audit recorded a historical `u16` distance-overflow bug that was subsequently fixed
in Rust with `u32` storage and insertion-time trapping. In the current Lean relations it
proves `SizeUp` entry preservation, the relation-level `RemapStep` entry result (it postulates `keySet` and `len`, so it is not a Rust refinement proof), base-write and
single-relocation-step invariant preservation, chain-maintenance lemmas, and the
conditional settled insert-chain theorem. It also proves the faithful bounded
`RemoveRelocate` chain preserves `ClusterInvariant` under `s.remapEnd = none`, and a
certificate-level settled found-branch bridge through the final public `len - 1` update.
The concrete lookup success direction is also proved for settled scans. A separate
occupancy certificate now proves the `len + 1` cardinality bridge for settled insert chains,
but does not yet derive its premises from Rust. Lookup completeness
is refuted by the current invariant's hole-permitting and length-independent model; it is now
proved conditionally under `NoHoles` and `LenCoherent`. Remaining work includes justifying both
strengthenings
from Rust/insertion history and proving the leading `remap_step` constructs the remove
certificate, handling active-remap and absent-key public branches, retiring the retained weak
`UnRelocateStep` declaration, and strengthening `RemapStep` before proving remap invariant
preservation. The weak `UnRelocateStep` has a relation counterexample for any inhabited
key domain and `RemapStep` is refuted by a machine-checked counterexample.
Neither finding establishes a Rust implementation defect. The target (c) theorem is
closed only at the abstract predicate level; actual lookup and persistence/re-open
refinement remain outside that proof.

## Status against "formal proof of a trustworthy data structure"

This is **not yet a complete** operation-level verification. `insert_preserves_invariant`
is conditional on a certified `InsertRelocateOK` settled chain and `remapEnd = none`; it
does not cover Rust certificate construction, active-remap insertion, or mid-chain
`size_up`. The two `sorry`s remain on the weak `UnRelocateStep` and `RemapStep`
declarations. The faithful bounded remove chain and its certificate-level settled
found-branch public-remove bridge are separately proved invariant-preserving under
`s.remapEnd = none`; the Rust justification of `NoHoles` and `LenCoherent`, construction of the occupancy and remove certificates from leading
`remap_step`, active-remap, absent-key, and persistence refinement remain outside those
theorems. The current `UnRelocateStep` relation has a counterexample for any inhabited key
domain, and the current `RemapStep` relation is false; neither relation finding establishes
a Rust defect.
The added stable-header helper and in-bounds theorem close only the remove
counterexample's header/geometry route; they remain distinct from the faithful chain and
do not close the weak `remove_preserves_invariant` declaration.
The proved target (c) predicate equivalence
does not cover the actual `lookupIndex`, persisted images, or `init` / re-open refinement,
so the structure should not be presented as formally verified end-to-end.

The model exposes its local assumptions explicitly: `SizeUp` records a cleared grown
region, `RemapStep` postulates `keySet` and `len`, leaves slot contents too weakly
constrained for invariant preservation, and is not a Rust refinement proof;
`RelocateWrite` specifies its distance, and `RelocateStep` has in-flight semantics. These
statements describe named relations, not an end-to-end or generic Rust-fidelity claim.
