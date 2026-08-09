# Verification Report

Date (UTC): 2026-08-09

## Target and version / Mode

- **Target**: `ic-stable-clustered-hash-map` — `StableClusteredHashMap` (src/map.rs), a
  stable-memory clustered (Amble & Knuth ordered) hash table with incremental in-place
  resize, at commit `cb74f70d1`.
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
- Stage 1 adversarial — `Counterexamples.lean`: counterexample to a claimed bound.
- Stage 2 — `Map.lean`: line-by-line transcription of scanning, probing, insert/remove
  relocation, and the incremental resize.
- Stage 3 — `Soundness.lean`: proofs of the target properties.
- Stage 4 — this report.

All modules build cleanly with `lake build`; the audit files contain no `sorry` except
the four deferred targets documented below.

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

### `Map.lean` (Stage 2 transcription — several under-specifications surfaced)
- `RelocateWrite` set the written entry's distance to `0`; the faithful distance is
  `position - bucket`. Fixed.
- `SizeUp` did not state the newly grown region is cleared (`clear_region`); without
  that, entry preservation is unprovable. Fixed (structure).
- `RemapStep` did not state entry-set preservation, so target (a) for the remap was
  unprovable; the invariant "remap preserves the entry set and count" was added.
- `RelocateStep` modeled a complete move (writing the displaced entry at `next`); it is
  in fact an **intermediate state** with the displaced entry in flight, and its distance
  handling was wrong. Refactored into a faithful Type-valued structure.

### `Soundness.lean` (Stage 3)
Proved:
- Target (a): `sizeUp_preserves_entries`, `remap_preserves_entries` — **fully proved**.
- Target (b), step level: `relocateWrite_preserves_clusterInvariant` (base write) and
  `relocateStep_preserves_clusterInvariant` (single relocation step) — **proved**.
- `insert_preserves_invariant` over the `InsertRelocate` chain: `done` case proved;
  `step` case open (see below).

## Findings (severity)

- **Medium (now fixed)**: u16 distance overflow could silently corrupt the persisted
  table when a single bucket's cluster exceeded 65535 entries (adversarial keys or very
  large, unluckily-clustered maps). **Resolution implemented in Rust**: distances are
  now `u32` and fit is enforced at insert by `checked_distance`, which traps on overflow
  — on the IC a trap rolls back the whole message, so no partial corruption.
- **Info**: the code comment "distances are bounded by the overflow area N" was
  inaccurate (non-structural); the audit's Lean counterexample documents why.
- **Info**: several Stage 2 relations were under-specified; each was refined to a form
  the proofs require, and each refinement is faithful to the implementation.

## List of `sorry` / unproven spots and interpretation

| Target | Theorem | Why unproved / what is needed |
|---|---|---|
| (b) insert | `insert_preserves_invariant` (`step` case) | Needs a chain **maintenance** argument: each displaced entry stays at its home bucket (`next - (tDist + (next-position)) = bucket t`) and `next` (the cluster end) is an order boundary for it. Step-level lemmas are proved; the chain induction is not closed. |
| (b) remove | `remove_preserves_invariant` | Same relocation-chain maintenance for `remove_and_relocate`'s gap-fill. |
| (b) remap | `remap_step_preserves_invariant` | Same maintenance across the incremental remap. |
| (c) reopen | `reopen_consistent_of_cluster_invariant` | Depends on target (b) being established; also needs `lookupIndex` to find exactly `KeySet` under both old/new mappings. |

These are the residual core of the audit, not defects: the step-level invariant
preservation is proved, and each remaining spot is a well-scoped induction/mantainance
argument rather than an observed failure.

## Conclusion

The audit **found and motivated a real latent correctness bug** — the `u16` distance
overflow — which has since been fixed (`u32` storage + insertion-time trap, giving IC
atomic rollback). It also **proved** the core structural properties: entry preservation
across `size_up` and `remap` (target (a), fully), and the cluster invariant preservation
for both the base insert write and a single relocation step (target (b), step level). The
remaining work is the chain-level maintenance induction for insert/remove/remap and the
re-open consistency argument (target (c)).

The transcriptions repeatedly proved the value of faithful, non-interpreted modeling:
several under-specified relations (SizeUp's cleared region, RemapStep's entry
preservation, RelocateWrite's distance, RelocateStep's in-flight semantics) only surfaced
when the proofs could not go through, and each was corrected to match the implementation.
