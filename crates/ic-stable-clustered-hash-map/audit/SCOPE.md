# Stage 0 — Scope (Lean Formal Audit of `StableClusteredHashMap`)

Date (UTC): 2026-08-10

## 1. Mode

**Audit mode.** An existing Rust implementation is transcribed into Lean and its
invariants are proved, surfacing where and why proofs fail and which assumptions are
required. No design-comparison mode.

## 2. Target components

The implementation under audit is `crates/ic-stable-clustered-hash-map/src/map.rs`
(primary), with `iter.rs`, `header.rs`, `memory.rs` cited only for the persistence
re-open invariant (target 3).

Components broken down by concern:

- **Table state**: slots `[0, capacity)` each holding a `distance: u16` (or
  `EMPTY = u16::MAX`) and, when occupied, a `(key, value)`; plus header fields
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

**(b) Cluster integrity.** After every operation the table satisfies the cluster
invariant: every occupied slot `i` has `distance(i) != EMPTY`, `bucket_i = i - distance(i)`
satisfies `bucket_i <= i`, and scanning slots in increasing order, `bucket_i` is
non-decreasing; each entry lies in the cluster of its bucket. This makes
`lookup_index` / `find_insert_position` correct and terminating. Distance fit in
`u32` is _not_ part of this structural invariant (see `Counterexamples.lean`); it is
enforced at insert by `checked_distance`, which traps on overflow.

**(c) Re-open mid-resize consistency.** A persisted state read back by `init` (header
`len`, `log2_buckets`, `remap_end` + slots) reconstructs a valid map; `lookup_index`
finds entries under both the old (mixed range `[0, remap_end]`) and new mappings, so
the key set is unchanged across re-open during a resize.

## 5. Assumptions / threat model

- Single-threaded execution (canister); no concurrency.
- Abstract memory `get`/`set` is correct; no corruption or external tampering of stable
  memory is modeled.
- `rapidhash` v3 is treated as a **deterministic** function `hash : Key → Nat`, assumed
  collision-free enough for the invariants to hold; the hash internals are **not**
  verified.
- Arithmetic bounds hold: `len`, `capacity` fit in `u64`; distances fit in `u16`
  (`EMPTY = u16::MAX` is never a real distance).
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
- `Map.lean` (Stage 2: transcription of the map logic)
- `Counterexamples.lean` (Stage 1 adversarial: B4 non-structural counterexample)
- Stage 3 proofs and the final `REPORT.md` to be added under `audit/`
