# vec-deque choice-free proofs and lean-verify matrix (2026-11-09)

Anchor timestamp: 2026-11-09 21:20:00 UTC +0900
Scope: plan 0380. Remove every `Classical.choice` dependency from the vec-deque block-ring proofs
and generalize `lean-verify.yml` into a matrix that builds and axiom-gates both lhm and vec-deque.
Parent commit: `a9b7b79f`. No Rust source, checker, other fixture or unrelated file changed.

## Choice roots and fixes

Traced with `#print axioms` from the guarded theorems down to core lemmas:

1. **`Nat.div_lt_of_lt_mul`** (core) depends on `Classical.choice`; it was used by
   `Svd.blockOf_lt` and the Grow routing bounds. Added a fixture-local constructive
   `Svd.div_lt_of_lt_mul'` in `Svd/Basic.lean` (quotient bound from `Nat.div_add_mod` and
   `Nat.mul_le_mul_left`) and replaced both uses.
2. **`flatRead_append_last`** pulled choice through a `simp`-generated `Nat.right_eq_add`
   instance in the `cons`/`zero` branch. Replaced that branch with an explicit
   `show`/`rw [if_neg …]`; also replaced the nil-cons length contradictions in
   `eq_of_length_eq_of_readAt`/`eq_of_length_eq_of_flatRead` (`simp at h`) with explicit
   `List.length_*` rewrites and `omega`. This cleared `contentUpTo_at`/`logicalList_at` and the
   `*_spec` theorems.
3. **Pop content/`Inv` proofs** closed contradiction-shaped `omega` goals via
   `Classical.byContradiction`. Replaced the nil-length occupancy proofs in
   `specPop_popBack_content`/`specPop_popFront_content` and the two `Inv` assembly occupancy
   lambdas with explicit `rwa [hn0] at hp` + `absurd … (Nat.not_lt_zero p)`.

Result: every guarded vec-deque theorem depends only on `propext`/`Quot.sound` (several on no
axioms); no `Classical.choice`, no `sorryAx`.

## Build and gate transcript

```text
$ cd crates/ic-stable-vec-deque/formal && lake build
Build completed successfully (14 jobs).          # no sorry warning

$ lake env lean Svd.lean | grep -E "sorryAx|Classical.choice"
(no matches; the guard lines show propext/Quot.sound only)

$ cd crates/ic-stable-linear-hash-map/formal && lake build && lake env lean Lhm.lean | grep -E "sorryAx|Classical.choice"
(no matches; lhm gate still passes)
```

Both local simulations of the CI axiom gate pass.

## CI matrix

`.github/workflows/lean-verify.yml` now runs one job matrix over:

| crate | module |
| --- | --- |
| `ic-stable-linear-hash-map` | `Lhm` |
| `ic-stable-vec-deque` | `Svd` |

Each job caches `~/.elan` + the crate's `formal/.lake`, installs elan, runs `lake build`, then
`lake env lean <module>.lean` and fails on `sorryAx|Classical.choice`. Path filters cover both
crates. The YAML parses with Ruby's `YAML.load_file`.

lara is intentionally outside the matrix: its surviving `unpack_pack_canonical` still depends on
`Classical.choice` (pre-existing), so adding it requires a separate cleanup.

## Freshness gate

Unchanged: all three fixtures anchored-clean.

```text
$ python3 scripts/check-lean-sources.py
lara [anchored …]      ok: 2 source(s) unchanged
lhm [anchored …]       ok: 4 source(s) unchanged
vec-deque [anchored …] ok: 1 source(s) unchanged
summary: 3 manifest(s), 0 blocking, 3 advisory/anchored-clean, 0 obsolete
```

## Documentation updates

- `Svd/SCOPE.md`: manifest anchored, CI matrix/choice-free note, Stage 5' remains.
- `Svd/REPORT.md`: axiom-cleanup record and CI wiring.
- ADR 0086/0104: choice-free + CI matrix notes; lara exclusion recorded.

## Remaining work

1. Stage 5' failure atomicity for vec-deque.
2. Clean lara's `unpack_pack_canonical` `Classical.choice` dependency, then add lara to the matrix.
