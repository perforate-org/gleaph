# Formal Verification Scope — ic-stable-linear-hash-map

Anchor timestamp: 2026-08-24 16:33:21 UTC +0000
Target revision: git `0da342d62b2a3c3b293fa7ff5ed21b9f577dd23d` (working tree of
`crates/ic-stable-linear-hash-map/` clean at this revision)

## Mode

**Audit mode against an existing implementation, run as a permanent fixture.**
This is not a one-shot audit report: `formal/` is a standing Lean project that is
re-checked with `lake build` whenever the map changes. Findings are recorded in
`REPORT.md`, which is refreshed alongside the proofs.

## Location and form

- Lean lake project: `crates/ic-stable-linear-hash-map/formal/`
- Independent from the Cargo workspace (no `Cargo.toml`; the workspace lists members
  explicitly, so this directory is invisible to Cargo).
- Toolchain: Lean 4 core + Std only (`leanprover/lean4:v4.33.1`). No Mathlib.
- Run locally: `cd crates/ic-stable-linear-hash-map/formal && lake build`.
- No CI wiring yet (repository has no `.github/workflows/`).

## Method

Hand-written Lean model plus proofs. Rust semantics are transcribed faithfully into
Lean definitions; each definition cites the source file and line range it mirrors
(function name plus line numbers at the target revision). The Rust-to-Lean generators
(Aeneas/Charon, hax) were considered and intentionally **not** used: the crate's core
paths are impure (`Memory` trait I/O, thread-local scratch buffers, external rapidhash),
which makes automated extraction high-effort and low-coverage for this target.

## Component breakdown (audit layers)

1. **Routing math** — `linear_bucket`, `base_buckets`, `next_geometry`,
   `split_threshold` and their interaction with the control-region shape.
2. **Control-region invariants** — the invariant set enforced by `validate_control`,
   validity of the initial control written by `create`, and preservation across split
   geometry transitions.
3. **Logical map specification** — insert/get/remove/clear/reset as abstract map
   operations over an abstract bucket array; `len` / `overflow_entries` accounting.
4. **Split preservation** — splits relocate entries only to `source` or
   `source + base(level)` and preserve the logical multiset of entries.
5. **Epoch fencing** — even/odd mutation epoch protocol; failure atomicity claims in
   ADR 0067 ("a prewrite error leaves logical bytes and the even epoch unchanged").

Stages 1–5 are **in scope and verified** (see Status); the roadmap below records
what each stage covers so it survives agent handoffs.

## Properties verified (stage 1–2 contract)

- P1 (route extent): `linear_bucket(h, level, cursor) < 2^level + cursor` holds
  unconditionally, for arbitrary hash values. Under the control invariant
  `physical_buckets = 2^level + split_cursor ∧ split_cursor < 2^level`, every routed
  access lands strictly inside the allocated bucket extent.
- P2 (split stability): under the standard linear-hash split, an entry's destination
  bucket is unchanged or moves by exactly the old base:
  - level increment (`level+1, 0`): new ∈ {old, old + 2^level};
  - cursor advance (`level, cursor+1`): new ∈ {old, old + 2^level}.
- P3 (geometry step validity): `next_geometry` applied to a valid geometry yields a
  valid geometry (cursor bound and bucket-count equation preserved), or fails closed
  (`CapacityOverflow`) at the level cap (< 63).
- P4 (control validity): the initial control written by `create` satisfies
  `ValidControl`; `ValidControl` mirrors every check performed by `open`'s
  `validate_control`.
- P5 (threshold monotonicity): `split_threshold` is nondecreasing in
  `physical_buckets` and bounded by the slot capacity.

## Assumptions (managed centrally here; axioms live only in Lean files with comments)

- A1 (hash opacity): rapidhash v3 is treated as an uninterpreted function. All routing
  properties hold for arbitrary hash values; no collision-resistance assumption is made
  or needed for P1–P3. Two-choice placement reasoning that *would* need collision
  assumptions belongs to stages 3–5 and must be declared there explicitly.
- A2 (sequential execution): one mutator at a time, matching IC canister message
  execution. The epoch protocol's cross-message recovery role is modeled in stage 5,
  not by concurrency assumptions.
- A3 (arithmetic domain): hash values, levels, cursors, and counts are mathematical
  naturals constrained to the u64 ranges the code enforces (`level < 63`,
  `split_cursor < 2^level`). Where Rust uses checked arithmetic that returns errors on
  overflow, the Lean model keeps exact arithmetic and models those errors as explicit
  failure variants; divergence direction is fail-closed in both.
- A4 (byte layer out of stage scope): page/block byte layout, occupancy words, and
  encode/decode round-trips are addressed in stage 3+, not by P1–P5.
- A5 (stage 3 abstraction): the model stores `Option (K×V)` per flattened slot index
  (`index = page*PRIMARY_SLOTS + slot`, map.rs `entries_from_image` enumeration) and
  assumes occupancy words always agree with stored bytes; the free-slot *choice* is
  generalized to "any free slot of the chosen candidate block" — the concrete
  first-free policy affects only physical scan order, formalized in stage 4.

## Out of scope

- `bench.rs`, canbench/candid features, PocketIC tests, Vector consumer integration.
- rapidhash internals.
- WASM memory implementation details of `ic-stable-structures::Memory`.
- Performance claims (covered by the benchmark contract in ADR 0067).

## Status

| Stage | Content | Status |
|---|---|---|
| 1–2 | Routing math + control invariants (P1–P5) | Verified, no `sorry` |
| 3a | Transfer principle + `setValue` (insert-update) preservation; occupancy-level `hisSome` abstraction (`Lhm/Abs/`) | Verified, no `sorry` |
| 3b | `placeAt` / `clearSlot` preservation via counter deltas and the generalized transfer core (`inv_transfer_core`) in `Lhm/Abs/Place.lean`, `Deltas.lean`, `Preserve.lean` | Verified, no `sorry` |
| 3c | Cleared-state preservation (`inv_cleared`, `inv_reset`, `Lhm/Abs/Cleared.lean`). Modeling decision: at the logical layer `clearedState` wipes every flattened slot; physical stale bytes beyond the initial extent (REPORT.md finding 4) are unreachable under any published control because a later `apply_split` writes complete block images before publishing growth — an ordering formalized in stage 5 as `write_before_publish` (with `cleared_published_empty`) | Verified, no `sorry` |
| 3d | Top-level operation contracts: `opInsert_preserves` / `opRemove_preserves` plus result-state computation lemmas and the free-slot choice fact (`Lhm/Abs/OpPreserve.lean`) | Verified, no `sorry` |
| 4 | Split preservation: `inv_split_transfer` in `Lhm/Abs/Split.lean` — one successful maintenance split (map.rs `plan_split` L1453-L1557 with `insert = none`, counters recomputed as in `finish_split_plan` L1575-L1620) preserves `Inv`. Covers candidate routing corollaries off the source (`route_fixed_step`), destination choice (`splitDest`), the re-packing transformer (`splitState` via `packImg`), load partition of the source block between the two images, and len/overflow counter recomputation. Placement and key-uniqueness are transported through the re-packed images; entries whose candidates miss both destinations model Rust's fail-closed `TablePressure` and are shown unreachable for old-source entries | Verified, no `sorry`; headline depends only on `propext`/`Quot.sound` |
| 5 | Epoch fencing / failure atomicity: guard transitions (`beginMutationAt`, `guardFinish`, `runGuarded`, `entryGate`), store-carrying failure atomicity per ADR 0067 (`run_guarded_fail_atomic`, `apply_split_call_fail_atomic`) with exact realization of the stage-3/4 committed states (`*_realizes`), entry-point fencing and cross-message recovery (`entry_gate_odd_fails`), write-before-publish byte layer (`write_before_publish`, `cleared_published_empty`), insert retry-loop progress measure (`remSplits`, `next_geometry_rem_splits`, `geom_chain_bounded`, `retry_loop_terminates`) in `Lhm/Abs/Epoch.lean` | Verified, no `sorry`; headline depends only on `propext`/`Quot.sound` |

Stage-4 modeling notes: Rust's `SPLIT_ENTRY_BUDGET`/`SPLIT_BYTE_BUDGET` rejection
(map.rs L1607-L1608) is not modeled — `Inv` does not constrain block-level budgets.
The `split_debt` recomputation (map.rs L1596–L1613) is likewise not modeled, so the
modeled debt of a carried insert follows the stage-3 convention (`debt_after_insert`,
map.rs L1680–L1691). A split that carries an insert (`plan_split(control, Some(…))`,
map.rs L953) was originally covered by neither stage 3 nor 4; it is now verified in
`Lhm/Abs/SplitInsert.lean` (see the stage-5 addendum's companion section in
REPORT.md).

Stage-5 modeling notes: call results carry the store in the failure branch too
(`CallResult.fail err state`), so "a prewrite error leaves logical bytes and the even
epoch unchanged" is a contentful equation rather than a vacuity — an unfaithful
transcription could return a half-written store on an error path and the atomicity
theorems would catch it. The guarded commit composes as begin (epoch → odd) → body →
close (epoch := observed + 2, written absolutely), which realizes every stage-3/4
transformer exactly; `splitState` inherits the epoch, so its realized state is
`{ splitState st g with mutationEpoch := st.mutationEpoch + 2 }`. The byte layer is
split into `RawStore` (physical, unclamped) and `publishedView` (clamped to the
published extent — what stages 3–4 model); finding 4's ordering appears as the
coverage hypothesis of `write_before_publish`: the only growth `nextGeometry` permits
is +1 and `apply_split` rewrites the newly exposed bucket before publishing. The u64
ceiling of `begin_mutation_at`'s `checked_add(2)` is modeled explicitly as `U64Max`
(`EpochExhausted`). Finding 5's progress measure is the closed form
`remSplits level cursor = 2^63 − (2^level + cursor)` (= `2^63 − physical_buckets`
under the control equation); it strictly decreases per successful split and at zero
`nextGeometry` fails closed.

Stage-3 files: `Lhm/Abs/Base.lean`, `State.lean`, `Ops.lean`, `Search.lean`,
`Transfer.lean`, `Deltas.lean`, `Preserve.lean`, `Place.lean`, `Cleared.lean`,
`OpPreserve.lean`.

Stage-4 file: `Lhm/Abs/Split.lean` (imports `Lhm.Abs.OpPreserve`; re-packing tools
`blockPred`/`scanTo`/`nthPack`/`packImg` and the additive counting lemmas live in
stage-3's `Base.lean`).

Stage-5 file: `Lhm/Abs/Epoch.lean` (imports `Lhm.Abs.Split`; guard transitions,
failure atomicity, the published-view byte layer, and the retry-loop measure).

Stage-4-note file: `Lhm/Abs/SplitInsert.lean` (imports `Lhm.Abs.Split` and
`Lhm.Abs.Epoch`; the insert-carrying split variant — picker bridge lemmas, key
absence across the split, `Inv` preservation, equivalence with plain split then
insert, and the guarded call wrapper).
