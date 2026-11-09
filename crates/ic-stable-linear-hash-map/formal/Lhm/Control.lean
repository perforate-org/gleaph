/-
Control-region invariants of the stable linear hash map.

Mirrors `ControlRegion` (header.rs L45-L56) and `validate_control` (map.rs L1047-L1068)
at audit target revision `9803f522e93edf26607492b4e15a0c6c508d58da`.

Re-anchored by plan 0376 (2026-11-09): `control.rs` now derives `level`/`split_cursor`
through `derive_geometry` (L48-L58), which is modelled in the section below. The existing
P1-P5 theorems are unchanged by that refactor: the routing/geometry/threshold functions they
describe are untouched, and `derive_geometry` only single-sources the derivation of fields
that `ValidControl` already constrains. `physical_buckets = 0` fails closed to `(0, 0)` outside
the validated domain instead of trapping.
-/
import Lhm.Routing

namespace Lhm

/-! ## Control region (header.rs L45-L56) -/

/-- Semantic mirror of the persisted 64-byte control record. Field order follows the
Rust struct; byte encode/decode is stage-3 work and deliberately not modeled here. -/
structure ControlRegion where
  len : Nat
  physicalBuckets : Nat
  mutationEpoch : Nat
  incarnation : Nat
  splitDebt : Nat
  overflowEntries : Nat
  level : Nat
  splitCursor : Nat

/-- **P4** invariant set: every check performed by `validate_control` (map.rs
L1047-L1068), with the odd-epoch case folded in as the quiescence requirement — an odd
`mutation_epoch` is rejected at open as `RecoveryRequired` (map.rs L1048-L1050) and
blocks mutations as `InProgress`. Level/cursor are derived fields on disk
(control.rs L59-L75 `decode`, via `derive_geometry` L48-L58); the invariant
constrains them through the bucket-count equation, which is what those
derivations reconstruct.

Conjunct order (for projections):
1. `InitialLevel ≤ level`
2. `level < 63`
3. `splitCursor < 2^level`
4. `physicalBuckets = 2^level + splitCursor`
5. `len ≤ physicalBuckets * SLOTS_PER_BUCKET`
6. `overflowEntries ≤ len`
7. `incarnation ≠ 0`
8. `mutationEpoch % 2 = 0`
-/
def ValidControl (c : ControlRegion) : Prop :=
  InitialLevel ≤ c.level
    ∧ c.level < 63
    ∧ c.splitCursor < 2 ^ c.level
    ∧ c.physicalBuckets = 2 ^ c.level + c.splitCursor
    ∧ c.len ≤ c.physicalBuckets * SlotsPerBucket
    ∧ c.overflowEntries ≤ c.len
    ∧ c.incarnation ≠ 0
    ∧ c.mutationEpoch % 2 = 0

/-- The exact control written by `create` (map.rs L401-L415):
`physical_buckets = INITIAL_BUCKETS = 2^INITIAL_LEVEL`, epoch 0, incarnation 1. -/
def initialControl : ControlRegion :=
  ⟨0, 2 ^ InitialLevel, 0, 1, 0, 0, InitialLevel, 0⟩

/-- **P4**: the initial control is valid, so a freshly created map opens. -/
theorem initialControl_valid : ValidControl initialControl := by
  unfold ValidControl initialControl InitialLevel SlotsPerBucket PrimarySlots PagesPerBucket
  dsimp only
  exact ⟨by decide, by decide, by decide, by decide, by decide, by decide, by decide,
    by decide⟩

/-- The two-choice routing corollary of P1 under open validation: for any hash value,
both candidate buckets of a validated map lie strictly inside the allocated extent.
This is the routing soundness fact the memory-safety argument rests on; grounding it in
byte offsets (`bucket_base`, map.rs L1917-L1919) is stage-3 work. -/
theorem route_in_extent (hash : Nat) (c : ControlRegion) (hv : ValidControl c) :
    linearBucket hash c.level c.splitCursor < c.physicalBuckets := by
  obtain ⟨_, _, hcur, hpb, _, _, _, _⟩ := hv
  have h := route_lt_base_plus_cursor hash c.level c.splitCursor
  omega

/-! ## Derived routing geometry (control.rs L48-L58 `derive_geometry`) -/

/-- Rust `derive_geometry`: floor-log2 level and residual cursor for a positive bucket count,
with `physical_buckets = 0` failing closed to `(0, 0)` (open-time validation makes that case
unreachable through a validated handle). -/
def deriveGeometry (physicalBuckets : Nat) : Nat × Nat :=
  if physicalBuckets = 0 then (0, 0)
  else
    (Nat.log2 physicalBuckets,
      physicalBuckets - 2 ^ Nat.log2 physicalBuckets)

/-- The fail-closed empty geometry (`physical_buckets == 0`). -/
theorem deriveGeometry_zero : deriveGeometry 0 = (0, 0) := by
  unfold deriveGeometry
  simp

/-- Positive bucket counts take the log2 + residual branch. -/
theorem deriveGeometry_pos {physicalBuckets : Nat} (h : physicalBuckets ≠ 0) :
    deriveGeometry physicalBuckets =
      (Nat.log2 physicalBuckets, physicalBuckets - 2 ^ Nat.log2 physicalBuckets) := by
  unfold deriveGeometry
  rw [if_neg h]

/-- **P4-derivation**: on a control that passes `validate_control`, `derive_geometry` reconstructs
exactly the persisted level/cursor equation (`physical_buckets = 2^level + split_cursor`,
`split_cursor < 2^level`); the `pb = 0` branch is outside the validated domain. -/
theorem deriveGeometry_of_valid (c : ControlRegion) (hv : ValidControl c) :
    deriveGeometry c.physicalBuckets = (c.level, c.splitCursor) := by
  obtain ⟨_, _, hcur, hpb, _, _, _, _⟩ := hv
  have hpb0 : c.physicalBuckets ≠ 0 := by
    rw [hpb]
    have hpos : 0 < 2 ^ c.level := two_pow_pos c.level
    omega
  rw [deriveGeometry_pos hpb0]
  have hlog : Nat.log2 c.physicalBuckets = c.level := by
    refine (Nat.log2_eq_iff hpb0).2 ⟨?_, ?_⟩
    · rw [hpb]; omega
    · rw [hpb]
      have hpow : 2 ^ (c.level + 1) = 2 ^ c.level + 2 ^ c.level := by
        rw [two_pow_succ_eq c.level]; omega
      omega
  rw [hlog]
  have hsub : c.physicalBuckets - 2 ^ c.level = c.splitCursor := by omega
  rw [hsub]
/-- A split geometry step taken from a valid control yields a well-formed successor:
P3 composed with the validity conjuncts. -/
theorem next_geometry_from_valid (c : ControlRegion) (hv : ValidControl c)
    (g : Geometry) (hg : nextGeometry c.level c.splitCursor c.physicalBuckets = some g) :
    g.cursor < 2 ^ g.level ∧ g.buckets = c.physicalBuckets + 1 :=
  next_geometry_shape c.level c.splitCursor c.physicalBuckets hv.right.right.left g hg

end Lhm
