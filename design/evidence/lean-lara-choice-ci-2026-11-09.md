# lara choice-free proofs and full lean-verify matrix (2026-11-09)

Anchor timestamp: 2026-11-09 21:35:00 UTC +0900
Scope: plan 0381. Remove the last `Classical.choice` dependency from the lara Lean fixture and add
lara to the `lean-verify.yml` matrix so all three anchored fixtures are CI-built and axiom-gated.
Parent commit: `6bb339fc`. No Rust source, checker, other fixture or unrelated file changed.

## Root cause and fix

`Lar.unpack_pack_canonical` was the only lara guard carrying `Classical.choice` (the others are
`propext`/`Quot.sound` or axiom-free). Its `simp [unpackVertexTail28, packVertexTail28, …]`
unfoldings expanded to `Int.toNat_eq_zero`, and that core lemma depends on `Classical.choice`.

Fix in `Lar/SlotIndex.lean`:

- the positive branch now calls a choice-free `unpack_pack_positive` helper, which computes both
  tombstone cases explicitly from `div2_mul2_add` (no `simp` over the Int tuple);
- the negative branch (and its `hpk` payload helper) now computes `unpackVertexTail28 0`/
  `unpackVertexTail28 1` by `cases tombstone <;> rw [hpk] <;> rfl`, again avoiding the
  `Int.toNat_eq_zero` expansion.

The theorem statements/claims are unchanged; only the proof terms changed.

## Build and gates

```text
$ cd crates/ic-stable-lara/formal && lake build
Build completed successfully (6 jobs).
$ lake env lean Lar.lean | grep -E "sorryAx|Classical.choice"
(no matches; the guard lines now show propext/Quot.sound only)
```

Local simulation of all three matrix jobs:

| project | module | gate |
| --- | --- | --- |
| `ic-stable-linear-hash-map` | `Lhm` | pass |
| `ic-stable-lara` | `Lar` | pass |
| `ic-stable-vec-deque` | `Svd` | pass |

## CI matrix

`lean-verify.yml` now has three entries (lara, lhm, vec-deque), with path filters for all three
crates and a cache key covering their `lean-toolchain` files. Each job runs `lake build`, then
`lake env lean <module>.lean` and fails on `sorryAx|Classical.choice`. YAML parses with Ruby
`YAML.load_file`.

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

- `Lar/SCOPE.md`: CI matrix/axiom-gate note.
- `Lar/REPORT.md`: dated axiom-cleanup section.
- ADR 0104: all three fixtures CI-gated; lara remains the first retirement candidate.

## Remaining work

1. Retire lara when its Verus migration criteria are met (ADR 0104).
2. Stage 5' failure atomicity for vec-deque.
