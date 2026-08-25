/-
Control-region invariants of the stable linear hash map.

Mirrors `ControlRegion` (header.rs L45-L56) and `validate_control` (map.rs L1047-L1068)
at audit target revision `0da342d62`.

Line citations were refreshed on 2026-08-24 (21:31 UTC) to the post-remediation source
after the level/cursor derivation was consolidated into control.rs `derive_geometry`;
the proofs are function-name-based and unaffected.
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
(control.rs L59-L73 `decode`, via `derive_geometry` L48-L57); the invariant
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

/-- A split geometry step taken from a valid control yields a well-formed successor:
P3 composed with the validity conjuncts. -/
theorem next_geometry_from_valid (c : ControlRegion) (hv : ValidControl c)
    (g : Geometry) (hg : nextGeometry c.level c.splitCursor c.physicalBuckets = some g) :
    g.cursor < 2 ^ g.level ∧ g.buckets = c.physicalBuckets + 1 :=
  next_geometry_shape c.level c.splitCursor c.physicalBuckets hv.right.right.left g hg

end Lhm
