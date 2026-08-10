# Stage 0 — Scope (Lean Formal Audit of `StableClusteredHashMap`)

Date (UTC): 2026-08-10
Anchor timestamp: 2026-08-10 21:30:00 UTC +0000

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
certificate remain open.

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

Lean artifacts under `audit/StableClusterAudit/` (a Lake project with Mathlib; see
`audit/lakefile.lean`):

- `Abstract.lean` (Stage 1: state model + invariants + assumptions)
- `Map.lean` (Stage 2: transcription of the map logic, including the bounded faithful
  `RemoveContinue` / `RemoveStop` / `RemoveRelocate` inner remove chain and compiler-checked
  stale-tail/header lemmas; the retained weak `UnRelocateStep` and its stable-header
  helper remain distinct)
- `Counterexamples.lean` (Stage 1 adversarial: B4 non-structural counterexample,
  `UnRelocateStep` relation counterexample for each supplied `k : Key`, and machine-checked
  refutation of invariant preservation by the current `RemapStep` relation)
- `Soundness.lean` (Stage 3: `sizeUp_preserves_entries` is proved for `SizeUp`; `remap_preserves_entries` is relation-level because `RemapStep` postulates `keySet` and `len`, not a Rust refinement proof of production remapping; target (b)'s certified settled insert chain is proved under `remapEnd = none`; predicate-level target (c) is proved through `EntryAtCorrectBucket`; `unrelocateStepWithStableHeader_preserves_inBounds` closes only the weak remove relation's header/geometry counterexample route; `removeRelocate_preserves_invariant` proves the faithful bounded remove chain under `s.remapEnd = none`; `publicRemoveSettled_preserves_invariant` proves the certificate-level settled found branch through the final `len - 1` update; `lookupIndex_some_implies_lookupFound` proves the settled lookup success direction but not completeness, and `publicRemoveSettled_lookupFound` forwards it through the public-remove certificate; the admitted `remove_preserves_invariant` still targets the weak relation, `remap_step_preserves_invariant` remains false under its weak relation, and exactly these two `sorry`s remain)
- `REPORT.md` (Stage 4: verification report — findings, severity, `sorry` interpretation)
