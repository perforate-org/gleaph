# Handoff: Closing the `insert_preserves_invariant` proof in Lean 4

**For**: a Lean 4 / dependent-type expert.
**Context**: a Lean formal audit of `StableClusteredHashMap`
(`crates/ic-stable-clustered-hash-map/`). The mathematics is complete; one theorem's
Lean encoding is blocked by a dependent-induction ergonomics issue.

---

## 1. Objective

Prove (close the `sorry` in) `insert_preserves_invariant` in
`audit/StableClusterAudit/Soundness.lean` — that a well-formed insert relocation chain
preserves `ClusterInvariant`. This is the last proof gap for the insert target of the
audit.

## 2. Repo / build

```
cd crates/ic-stable-clustered-hash-map/audit
lake -d . build StableClusterAudit.Abstract StableClusterAudit.Map \
  StableClusterAudit.Counterexamples StableClusterAudit.Soundness
```
- Lake project with **Mathlib v4.32.2** (`lean-toolchain` = `leanprover/lean4:v4.32.2`).
- The build is green at the last committed state (`9b768b096`); `insert_preserves_invariant`
  is the only `sorry` in the insert target (there are also `sorry`s for
  `remove_preserves_invariant`, `remap_step_preserves_invariant`,
  `reopen_consistent_of_cluster_invariant`; those are follow-on work).

## 3. What is already proved (all in `Soundness.lean`)

- Target (a): `sizeUp_preserves_entries`, `remap_preserves_entries` — entry set/count
  preserved across resize and remap. **Fully proved.**
- Step level: `relocateWrite_preserves_clusterInvariant` and
  `relocateStep_preserves_clusterInvariant` — the empty-slot write and a single relocation
  step each preserve `ClusterInvariant`.
- Chain-maintenance machinery (all proved):
  - `displaced_home_bucket` — a displaced entry stays at its home bucket.
  - `bucketAt_in_scan` (+ `_aux`) — a slot inside the recursive `endOfClusterFrom` scan is
    at the scanned bucket (proved by strong induction on the scan length).
  - `endOfClusterFrom_ge`, `endOfClusterFrom_le_capacity` — scan bounds.
  - `order_boundary_of_cluster_end` — the end of a cluster is an order boundary for its
    bucket.
  - `relocateStep_preserves_order_boundary` — the boundary survives a relocation step.

Key model definitions (in `Map.lean`):
- `InsertRelocate : State → State → Key → Nat → Nat → Prop` — inductive chain of
  `RelocateStep`s ended by a `RelocateWrite` (`done`/`step` constructors).
- `RelocateStep` (a faithful Type-valued structure carrying `entryDist`, the displaced
  entry `tKey/tVal/tDist`, `next`, and a `remapEnd`-preservation field).
- `ClusterInvariant` = `DistanceValid ∧ ClusterOrdered ∧ EntryAtCorrectBucket`
  (in `Abstract.lean`).

## 4. The blocking theorem

```lean
lemma insert_preserves_invariant (hok : InsertRelocateOK) :
    ∀ {s s' : State} {key : Key} {value : Nat} {position : Nat}
      (h : InsertRelocate s s' key value position) (hrel : OkRelates h hok)
      (hci : ClusterInvariant s) (hremap : s.remapEnd = none)
      (hremap' : s'.remapEnd = s.remapEnd),
    ClusterInvariant s' := by
  induction hok with
  | done hw hslot hbound => ...  -- PROVED
  | step mid entryDist hstep hnext hbound hbucket _hprec hok_next => ...  -- BLOCKED
```

The `step` case needs, with `hrel` from `OkRelates h hok`:
1. `hci_mid : ClusterInvariant mid` from
   `relocateStep_preserves_clusterInvariant hstep hci hbound hremap hstep.remapEnd hbucket`,
2. `hremap_mid : mid.remapEnd = none`, `hremap'_mid : s'.remapEnd = mid.remapEnd`,
3. the recursive hypothesis applied to the continuation `hnext`/`hok_next`/`hrel'`.

All the mathematical facts are in hand; the obstacle is purely **Lean's dependent
induction**: the recursive hypothesis and the `cases hrel` variables fail to align.

## 5. The exact Lean problem

`InsertRelocateOK` (indexed form, committed state) is
`InsertRelocateOK : {s s'} {key} {value} {position} → InsertRelocate s s' key value position → Prop`,
indexed by a **value** of another inductive. The recursion changes the indices
(`s → mid`, `key → hstep.tKey`, `position → hstep.next`), so:

- `induction hok` marks the recursive hypothesis inaccessible (`hok_ih✝`); Lean cannot
  expose it because the motive generalizes the indices.
- A recursive `def` that ties the chain to a certificate
  (`OkRelates hnext hok` with changed indices) fails to compile:
  > `the dependent pattern matcher can solve the following kinds of equations ...`
  (a `HEq`/`≍` obligation it cannot discharge).

## 6. What was already tried (and why each fell short)

1. **`induction hok`** over the indexed `InsertRelocateOK h` — IH inaccessible (`✝`).
2. **`induction hok using InsertRelocateOK.rec`** — the IH is still parameterized over the
   generalized indices and not directly applicable.
3. **Non-indexed certificate** — `InsertRelocateOK : Prop` with states/chain carried as
   constructor fields; the recursive certificate `hok : InsertRelocateOK` is then
   non-indexed and its IH *is* accessible. Good first step.
4. **`OkRelates` as an inductive (Prop)** instead of a recursive `def` — this removes the
   HEq in the definition (the constructors align `InsertRelocate.done/step` with
   `InsertRelocateOK.done/step` structurally). Good first step.
5. **`induction hok` (non-indexed) + `cases hrel` (inductive `OkRelates`)**: the `step`
   case still fails — `cases hrel` introduces fresh metavariables (`mid✝`, `hstep✝`,
   `hnext✝`) that are only **HEq**, not **definitionally equal**, to the induction's
   `mid`/`hstep`/`hnext`, so applying the IH `ih hnext hrel' ...` gives an application
   type mismatch. The current working tree was reverted to the committed indexed state
   (`9b768b096`) because the re-encode did not compile cleanly.

## 7. Suggested directions for the expert

The mathematics is done; pick the Lean encoding that makes the induction tractable:

- **(Recommended) Carry the chain in the certificate, drop `OkRelates`.** Make
  `InsertRelocateOK : Prop` non-indexed and have each constructor carry the corresponding
  `InsertRelocate` chain (`h : InsertRelocate s s' key value position`) plus the
  well-formedness facts. Then `induction hok` gives the chain data directly, no separate
  relation, and the recursive `hok` field is non-indexed → IH accessible.
  Trade-off: you must also record `h = InsertRelocate.step mid entryDist hstep hnext`
  (or reconstruct it) so the theorem's `h` and the certificate agree.
- **Induct on `hrel` (the `OkRelates` certificate) instead of `hok`**, so `cases`/`induction`
  align on one inductive; or align the `cases hrel` metavariables to the induction's with
  `subst`/`simpa` so the recursive call is applied with definitionally equal terms.
- **Avoid `cases` inside `induction`**: restructure so the step case needs only one
  destructor, e.g. by making `OkRelates.step`'s data definitionally match the induction
  pattern, or by deriving the needed facts (not `hrel`) with `rcases`/`inversion` that
  `subst`s the shared variables.
- **Well-founded recursion** on the certificate depth (`termination_by`) instead of
  `induction`, giving an explicit recursive call you can invoke with the continuation's
  hypotheses.

## 8. Follow-on work after `insert_preserves_invariant`

- `remove_preserves_invariant` (`UnRelocateStep` gap-fill chain),
- `remap_step_preserves_invariant` (incremental remap),
- `reopen_consistent_of_cluster_invariant` (target (c); lookup finds exactly `KeySet`).

## 9. Deliverables / state

- `audit/REPORT.md` — the verification report (findings, severity, `sorry` interpretation,
  an explicit "status against a trustworthy formal proof" section).
- `audit/SCOPE.md`, `audit/StableClusterAudit/{Abstract,Map,Counterexamples,Soundness}.lean`.
- Working tree is clean at `9b768b096`; all four modules build.
