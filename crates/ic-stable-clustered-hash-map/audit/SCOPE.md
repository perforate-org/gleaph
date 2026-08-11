# Stage 0 — Scope (Lean Formal Audit of `StableClusteredHashMap`)

Date (UTC): 2026-08-10
Anchor timestamp: 2026-08-11 06:17:35 UTC +0000

## 1. Mode

**Audit mode.** An existing Rust implementation is transcribed into Lean and its
invariants are proved, surfacing where and why proofs fail and which assumptions are
required. No design-comparison mode.

## 2. Target components

The implementation under audit is `crates/ic-stable-clustered-hash-map/src/map.rs`
(primary), with `iter.rs`, `header.rs`, `memory.rs` cited only for the persistence
re-open invariant (target 3).

Components broken down by concern:

- **Table state**: slots `[0, capacity)` each holding a `distance: u32` (or
  `EMPTY = u32::MAX`) and, when occupied, a `(key, value)`; plus header fields
  `len`, `log2_buckets` (`n`), `remap_end`.
- **Cluster / distance model**: `bucket_i = i - distance(i)`; clustering and the
  non-decreasing `bucket_i` ordering that makes probing correct and terminating.
- **Mutations**: `insert` / `insert_and_relocate` / `remove` /
  `remove_and_relocate` / `find_insert_position` / `lookup_index`.
- **Incremental resize**: `size_up` + `remap_step` + `remap_position`
  (mixed-range boundary `remap_end`).

## 3. Location of inputs

- `crates/ic-stable-clustered-hash-map/src/map.rs` — all core logic.
- `crates/ic-stable-clustered-hash-map/src/iter.rs`, `header.rs`, `memory.rs` — only
  where needed for the re-open invariant (target 3).

## 4. Properties to verify

**(a) Entry preservation across resize.** For any sequence of `insert(k, v)` /
`remove(k)` operations (each preceded by the bounded `remap_step(REMAP_BATCH)`, with
`size_up` triggered when `len >= 3/4 * buckets`), the map's entry set equals the set
expected from the operation sequence. In particular, `size_up` + `remap_step` +
`remap_position` must not lose, duplicate, or misplace any entry.
In the current Lean status, `sizeUp_preserves_entries` proves the `SizeUp` relation, while `remap_preserves_entries` is relation-level because `RemapStep` postulates `keySet` and `len`; it is not a Rust refinement proof of production `remap_step` or `remap_position`.

**(b) Cluster integrity.** After every operation the table satisfies the cluster
invariant: every occupied slot `i` has `distance(i) != EMPTY`, `bucket_i = i - distance(i)`
satisfies `bucket_i <= i`, and scanning slots in increasing order, `bucket_i` is
non-decreasing; each entry lies in the cluster of its bucket. This makes
`lookup_index` / `find_insert_position` correct and terminating. Distance fit in
`u32` is _not_ part of this structural invariant (see `Counterexamples.lean`); it is
enforced at insert by `checked_distance`, which traps on overflow.
The current weak `UnRelocateStep` and `RemapStep` relations do not constrain enough state
to preserve this invariant. `remapStep_does_not_preserve_clusterInvariant` refutes
`remap_step_preserves_invariant`; for any supplied `k : Key` (and thus an inhabited key
domain), `unrelocateStep_does_not_preserve_clusterInvariant` refutes
`remove_preserves_invariant`. These are Lean relation defects, not Rust implementation
defects or evidence that the modeled counterexample is Rust-reachable.
`UnRelocateStepWithStableHeader` and the compiler-checked
`unrelocateStepWithStableHeader_preserves_inBounds` theorem close only that remove
counterexample's header/geometry route. The helper itself does not model faithful
continue/stop/chain behavior, does not close `remove_preserves_invariant`, and does not
change the two remaining `sorry`s. Separately, `SameRemoveHeader`, `RemoveFrame`,
`RemoveContinue`, `ClearCurrentHole`, `RemoveStop`, and inductive `RemoveRelocate` now
model the bounded inner gap-fill chain. The kernel-checked
`removeRelocate_preserves_invariant` theorem proves that faithful chain preserves
`ClusterInvariant` when `s.remapEnd = none`. The machine-checked
`removeRelocate_activeBoundary_counterexample` shows why that settled premise cannot be
dropped under the current `ExpectedBucket` invariant.
The certificate-level `PublicRemoveSettled` relation and
`publicRemoveSettled_preserves_invariant` theorem connect that settled chain to the found
branch's final `len - 1` update and preserve `n` and `remapEnd`; they consume modeled
lookup/relocation certificates and do not prove the leading `remap_step`, concrete lookup
construction, active-remap or absent-key branches, or persistence refinement.
The settled scan lemma `lookupIndex_some_implies_lookupFound` proves the success direction
for the modeled concrete lookup: a returned slot is in bounds, occupied, contains the key,
and has the expected bucket. Lookup completeness and construction of the public-remove
certificate remain open. The machine-checked
`lookupIndex_completeness_counterexample` shows why: the current `ClusterInvariant` allows
an empty slot before an occupied entry, while `scanFor` stops at that empty slot. A
no-holes / scan-contiguity strengthening or insertion-history relation is required.
The explicit `NoHoles` strengthening and
`lookupIndex_complete_of_noHoles` theorem now prove the completeness direction for
settled states when `LenCoherent` is supplied; `len_pos_of_lenCoherent_keySet` derives
the positive-length fact needed by the scan. The Rust/insertion-history derivation of
`NoHoles` and `LenCoherent` is still outside the audit.
`clusterInvariant_does_not_imply_len_positive` machine-checks that the length/occupancy
linkage cannot be inferred from the current invariant and key set.
`InsertRelocateOccupancyOK` and `insertRelocate_preserves_occupiedCard` now provide a
separate certified cardinality bridge for settled insert chains, and
`publicInsertSettled_preserves_lenCoherent` proves the final `len + 1` update preserves
`LenCoherent` when the chain header length is supplied. The in-bounds/non-empty occupancy
facts and header relation are deliberately explicit: they are not yet derived from
`InsertRelocateOK` or Rust insertion history.
`relocateWrite_preserves_noHoles` proves the terminating insert write preserves `NoHoles`
when the insertion-point prefix is explicitly occupied. The compiler-checked
`findInsertPositionFrom_prefix` / `findInsertPosition_prefix` lemmas extract the occupied
prefix traversed by the initial Rust `find_insert_position` scan. Per-step displaced-entry
`relocateStep_next_prefix_of_noHoles` derives the next pending entry's occupied prefix from
`NoHoles` and the `endOfCluster` scan when the current position and pending distance are
bounded. `insertRelocateNoHolesOK_of_occupancyOK` and
`insertRelocate_preserves_noHoles_of_occupancyOK` thread that conditional prefix through a
complete chain when `InsertRelocateOccupancyOK` is supplied. Deriving the occupancy/bound
certificate from Rust remains open.
`relocateStep_preserves_noHoles` now proves the intermediate relocation write under the
same explicit prefix and non-EMPTY distance premises; the pending displaced entry is not
part of the intermediate state. `InsertRelocateNoHolesOK` and
`insertRelocate_preserves_noHoles` compose the full certified chain; only the derivation of
each displaced-entry prefix/distance from Rust remains open.
`InsertRelocateNoHolesOK` and `insertRelocate_preserves_noHoles` now compose those cases
across the full certified insert chain. The initial scan prefix is derived by
`findInsertPositionFrom_prefix` / `findInsertPosition_prefix`; per-step prefix and distance
premises remain explicit. `relocateStep_next_prefix_of_noHoles` supplies the conditional
next-step prefix, and `insertRelocateNoHolesOK_of_occupancyOK` propagates it through the
chain. Rust's distance/bound derivation and certificate construction remain open.
`insertRelocate_preserves_noHoles_of_findPosition` supplies the initial prefix from an
explicit `findInsertPosition` result and closes the complete-chain `NoHoles` bridge when
the source `NoHoles` and occupancy certificate are supplied.
`InsertRelocateTrace` is now a separate no-resize certificate for the bounded inner insert
loop. It records `position < capacity`, `pendingDist < EMPTY`, the direct
`RelocateStep` tie, the exact next pending-distance equation, strict progress and strict
next-capacity bounds, and terminal distance fidelity. `Soundness.lean` proves that this
trace projects to `InsertRelocate` and `InsertRelocateOccupancyOK`; it does not prove that
Rust constructs the trace or refine public insert. The outer `size_up` path and initial
checked distance, `InsertRelocateOK` ordering facts, `NoHoles`/`LenCoherent`, header/remap
facts, active remapping, and the absent-key branch remain outside this bounded slice.
The abstract `freshState` models the cleared table after Rust `new`; its invariant,
no-holes, length-coherence, and empty-key-set lemmas close only the initial base state,
not persisted header validation or `init`.

**(c) Re-open mid-resize consistency.** A persisted state read back by `init` (header
`len`, `log2_buckets`, `remap_end` + slots) reconstructs a valid map; `lookup_index`
finds entries under both the old (mixed range `[0, remap_end]`) and new mappings, so
the key set is unchanged across re-open during a resize.
The current Lean theorem is kernel-checked only at the abstract predicate level: it
proves `KeySet` equivalence with the existential `LookupFound` predicate through
`EntryAtCorrectBucket`, not a refinement of the actual `lookupIndex`, a persisted memory
image, or `init` / re-open behavior.

## 5. Assumptions / threat model

- Single-threaded execution (canister); no concurrency.
- Abstract memory `get`/`set` is correct; no corruption or external tampering of stable
  memory is modeled.
- `rapidhash` v3 is treated as a **deterministic** function `hash : Key → Nat`, assumed
  collision-free enough for the invariants to hold; the hash internals are **not**
  verified.
- Arithmetic bounds hold: `len`, `capacity` fit in `u64`; distances fit in `u32`
  (`EMPTY = u32::MAX` is never a real distance).
- The documented aliasing rule (`&self` mutation, no aliasing while an iterator is
  alive) is honored by callers.

## 6. Out of scope

- Byte-level layout, header magic, and layout version.
- Memory growth / allocation (`grow_memory_to_at_least_bytes`) and the
  `OutOfMemory` / `InsertError` error paths.
- `Storable::to_bytes` internals (treated as injective).
- Iteration ordering guarantees.
- Benchmarks, README, and non-implementation files.

## 7. Deliverables

Final validation for this bounded trace slice succeeded: `lake build
StableClusterAudit.Map`, `lake build StableClusterAudit.Soundness`, and `lake build
StableClusterAudit.Counterexamples` each exited 0; `Soundness` retained only the two
pre-existing `sorry` warnings, and `git diff --check` exited 0.

Lean artifacts under `audit/StableClusterAudit/` (a Lake project with Mathlib; see
`audit/lakefile.lean`):

- `Abstract.lean` (Stage 1: state model + invariants + assumptions, including the explicit
  `NoHoles` and `LenCoherent` strengthenings required for lookup completeness and the
  cleared `freshState` base corresponding to Rust `new`)
- `Map.lean` (Stage 2: transcription of the map logic, including the bounded faithful
  `RemoveContinue` / `RemoveStop` / `RemoveRelocate` inner remove chain and compiler-checked
  stale-tail/header lemmas; the retained weak `UnRelocateStep` and its stable-header
  helper remain distinct; `InsertRelocateOccupancyOK` records the explicit slot facts needed
  for the length/cardinality bridge, and `InsertRelocateNoHolesOK` records the per-step scan
  prefixes needed for full-chain `NoHoles` preservation; the independent no-resize
  `InsertRelocateTrace` records current/next bounds, pending-distance fidelity, strict
  progress, and the exact displacement-distance transition)
- `Counterexamples.lean` (Stage 1 adversarial: B4 non-structural counterexample,
  `UnRelocateStep` relation counterexample for each supplied `k : Key`, machine-checked
  refutation of invariant preservation by the current `RemapStep` relation, and the
  `lookupIndex_completeness_counterexample` hole/scan counterexample plus
  `clusterInvariant_does_not_imply_len_positive` length-coherence counterexample)
- `Soundness.lean` (Stage 3: `freshState_clusterInvariant`, `freshState_noHoles`, `freshState_lenCoherent`, and `freshState_keySet_empty` close the cleared `new` base; `sizeUp_preserves_entries` is proved for `SizeUp`; `remap_preserves_entries` is relation-level because `RemapStep` postulates `keySet` and `len`, not a Rust refinement proof of production remapping; target (b)'s certified settled insert chain is proved under `remapEnd = none`; `insertRelocate_preserves_occupiedCard` proves the occupancy change for the explicit insert certificate, `publicInsertSettled_preserves_lenCoherent` proves the final `len + 1` bridge, `relocateWrite_preserves_noHoles` / `relocateStep_preserves_noHoles` prove the terminating and intermediate no-holes cases, `insertRelocate_preserves_noHoles` composes them across the certified chain, and `findInsertPositionFrom_prefix` / `findInsertPosition_prefix` extract the initial occupied prefix from the Rust scan; `relocateStep_next_prefix_of_noHoles` derives a conditional next-step prefix from `NoHoles` and `endOfCluster`, `insertRelocateNoHolesOK_of_occupancyOK` / `insertRelocate_preserves_noHoles_of_occupancyOK` propagate it through a chain with `InsertRelocateOccupancyOK`, and `insertRelocate_preserves_noHoles_of_findPosition` closes the scan-to-chain `NoHoles` bridge under an explicit occupancy certificate; Rust source-history `NoHoles`, distance/bound/occupancy certificate construction remains open; predicate-level target (c) is proved through `EntryAtCorrectBucket`; `unrelocateStepWithStableHeader_preserves_inBounds` closes only the weak remove relation's header/geometry counterexample route; `removeRelocate_preserves_invariant` proves the faithful bounded remove chain under `s.remapEnd = none`; `publicRemoveSettled_preserves_invariant` proves the certificate-level settled found branch through the final `len - 1` update; `lookupIndex_some_implies_lookupFound` proves the settled lookup success direction, `publicRemoveSettled_lookupFound` forwards it through the public-remove certificate, and `lookupIndex_complete_of_noHoles` proves completeness under explicit `NoHoles` and `LenCoherent`; the Rust justification of those assumptions remains open; the admitted `remove_preserves_invariant` still targets the weak relation, `remap_step_preserves_invariant` remains false under its weak relation, and exactly these two `sorry`s remain)
- `REPORT.md` (Stage 4: verification report — findings, severity, `sorry` interpretation)
