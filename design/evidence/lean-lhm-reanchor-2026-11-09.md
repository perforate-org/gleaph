# lhm Lean fixture re-anchor (2026-11-09)

Anchor timestamp: 2026-11-09 19:30:00 UTC +0900
Scope: plan 0376. Re-anchor the lhm Lean fixture from `0da342d6` to the current `control.rs`/
`map.rs` surface, formalize the `derive_geometry` single-source helper, then flip the freshness
status to `anchored` (blocking). Parent commit: `9803f522e93edf26607492b4e15a0c6c508d58da`.
No Rust source, test, CI workflow or other fixture changed.

## Drift and the P3–P5 question

The classification record asked which `control.rs` changes invalidate P3–P5. Answer: **none**. The
drift since the anchor is small and is a refactor, not a contract change:

- `control.rs` derives `(level, split_cursor)` through `derive_geometry(physical_buckets)`
  (L48-L58), called from `decode` (L59-L75) and `read_hot_with_epoch`; `physical_buckets == 0` now
  fails closed to `(0, 0)` instead of trapping or saturating. `map.rs` routes `scrub_control`
  through the same helper (the only map.rs change).
- `next_geometry` (map.rs L1693-L1709), `split_threshold` (L1711-L1717), `base_buckets`
  (L1719-L1721) and `validate_control` (L1047-L1068) are byte-for-byte unchanged, so the P3
  geometry-shape properties and P5 threshold properties are untouched. `ValidControl` already
  requires `physical_buckets = 2^level + split_cursor ∧ split_cursor < 2^level`, which excludes the
  new `pb = 0` branch from the validated domain, so P4 is also unaffected.
- The model's citations had already been refreshed to the post-refactor working tree while its
  recorded anchor said `0da342d6`; that record/anchor mismatch is what the re-anchor fixes.

## Model diff

`crates/ic-stable-linear-hash-map/formal/Lhm/Control.lean` (existing theorems/proofs untouched):

- `deriveGeometry : Nat → Nat × Nat` — `pb = 0` → `(0, 0)`; otherwise `Nat.log2 pb` and
  `pb - 2 ^ Nat.log2 pb` (Rust `derive_geometry`, L48-L58);
- `deriveGeometry_zero`, `deriveGeometry_pos`;
- `deriveGeometry_of_valid` — **P4-derivation**: for a `ValidControl`, the helper returns exactly
  `(level, splitCursor)`, using `Nat.log2_eq_iff` with `2^level ≤ pb < 2^(level+1)`;
- citations refreshed: `decode` L59-L73 → L59-L75, `derive_geometry` L48-L57 → L48-L58; header
  audit revision updated and the refactor/P3–P5 note added.

`Lhm/Routing.lean` gets the audit-revision header only; `Lhm.lean` gains
`#print axioms Lhm.deriveGeometry_of_valid`. `Lhm/Abs/**` and `Lhm/Basic.lean` are untouched.

## Build and axiom transcript

```text
$ cd crates/ic-stable-linear-hash-map/formal && lake build
Build completed successfully (19 jobs).

$ lake env lean Lhm.lean   # grep -E "sorryAx|Classical.choice"
'Lhm.deriveGeometry_of_valid' depends on axioms: [propext, Quot.sound]
(no sorryAx / Classical.choice matches; exit 0)
```

The existing 38 headline theorems still print the same axiom sets as before the re-anchor; the new
theorem uses only `propext` and `Quot.sound`.

## Freshness gate

`crates/ic-stable-linear-hash-map/formal/SOURCES.sha256` now reads:

```
# status: anchored
# anchor: 9803f522e93edf26607492b4e15a0c6c508d58da
# reviewed: 2026-11-09
ed16bf5f…  crates/ic-stable-linear-hash-map/src/control.rs
b38b2449…  crates/ic-stable-linear-hash-map/src/header.rs
a59b2015…  crates/ic-stable-linear-hash-map/src/map.rs
7881cccc…  crates/ic-stable-linear-hash-map/src/memory.rs
```

Checker transcript:

```text
$ python3 scripts/check-lean-sources.py
fixture crates/ic-stable-lara/formal/SOURCES.sha256 [anchored anchor d75d2467…]
  ok: 2 source(s) unchanged
fixture crates/ic-stable-linear-hash-map/formal/SOURCES.sha256 [anchored anchor 9803f522…]
  ok: 4 source(s) unchanged
fixture crates/ic-stable-vec-deque/formal/SOURCES.sha256 [obsolete …]
  skipped: fixture models deleted code (see its SCOPE.md)
summary: 3 manifest(s), 0 blocking, 2 advisory/anchored-clean, 1 obsolete
```

Both surviving transcription fixtures are now blocking; any future drift in their cited sources turns
the checker red until the model is re-read and the hashes are refreshed together.

## Documentation updates

- `formal/SCOPE.md`: target revision `9803f522…`, freshness status `anchored`/blocking, re-anchor
  procedure.
- `formal/REPORT.md`: dated re-anchor section with the P3–P5 answer, helper model and axiom result.
- ADR 0104: implementation note recording lhm as anchored/blocking; only vec-deque remains obsolete.

## Remaining work (later slices)

1. Restate the vec-deque model for the ADR 0086 segmented ring and give it a fresh manifest.
2. Model the lhm scrub path / byte-level control codec if the audit wants it, or retire the fixture
   when the Verus migration criteria are met (ADR 0104).
