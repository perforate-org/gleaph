# lara Lean fixture re-anchor (2026-11-09)

Anchor timestamp: 2026-11-09 19:05:00 UTC +0900
Scope: plan 0375. Re-anchor the surviving lara Lean models from `dbd76f3d` to the current
`slab_index.rs` surface, then flip the fixture's freshness status to `anchored` (blocking).
Parent commit: `d75d246789f628d19e8821a41a9881d151c7a6fa`. No Rust source, test, CI workflow or other
fixture changed.

## Drift scope since the old anchor

`src/labeled/bucket_label_key.rs` is byte-identical to `dbd76f3d` (`5f352ac2…`), so its model needed
no change. `src/slab_index.rs` changed (`ddfd8168…` → `67a7d4ef…`); the changes that mattered to the
model were vocabulary and citations, not arithmetic:

- the LogHead codec and `OVERFLOW_LOG_NONE` were deleted by plan 0372 (the model never used them);
- the vertex tail28 payload is now named a **spill run** (`VERTEX_TAIL_RUN_MASK`,
  `pack_vertex_tail28(spill_run, …)`, `unpack_vertex_tail28`, `try_pack_vertex_tail28`) while the
  numeric encoding is unchanged: bit 0 tombstone, bits 1–27 hold `(run + 1)`, `0` = no run;
- every line citation in the model was stale (the file is now 239 lines);
- newer helpers (`checked_add_byte_exclusive_end`, `checked_add_slot_exclusive_end`,
  `slot_exclusive_end_fits`, `validate_elem_capacity`, `read_u40`/`write_u40`) are **out of scope**:
  the re-anchor refreshes the existing theorem surface, it does not extend it.

## Model changes

`crates/ic-stable-lara/formal/Lar/SlotIndex.lean`:

- `vertexTailLogMask` → `vertexTailRunMask` (and its lemma name);
- `logHead` parameters → `spillRun`; "log head" / "no log" wording → "spill run" / "no spill run";
- the file docstring and all section/function citations refreshed to the current lines
  (constants L4–L25/L158–L160, checked addition L52–L56/L119–L123, locator words L64–L108, vertex
  tail28 L154–L199).

Theorem statements and proof scripts are otherwise unchanged; the four headline theorem names
(`pack2_inj`, `checkedAddSlotIndex_spec`, `locator_decode_of_tryEncode`, `unpack_pack_canonical`) and
their claims are the same as before. `Lar/Basic.lean`, `Lar/BucketLabelKey.lean` and `Lar.lean` were
not edited.

## Build and axiom transcript

```text
$ cd crates/ic-stable-lara/formal && lake build
✔ [3/6] Built Lar.SlotIndex (4.3s)
✔ [4/6] Built Lar.BucketLabelKey (855ms)
ℹ [5/6] Built Lar (524ms)
info: Lar.lean:14:0: 'Lar.pack2_inj' depends on axioms: [propext, Quot.sound]
info: Lar.lean:15:0: 'Lar.checkedAddSlotIndex_spec' depends on axioms: [propext, Quot.sound]
info: Lar.lean:16:0: 'Lar.locator_decode_of_tryEncode' depends on axioms: [propext, Quot.sound]
info: Lar.lean:17:0: 'Lar.unpack_pack_canonical' depends on axioms: [propext, Classical.choice, Quot.sound]
Build completed successfully (6 jobs).

$ lake env lean Lar.lean   # grep sorryAx
(no output; exit 0)
```

The axiom set is identical to the pre-re-anchor build. `Classical.choice` in
`unpack_pack_canonical` predates this slice and is recorded here rather than silently changed.

## Freshness gate

`crates/ic-stable-lara/formal/SOURCES.sha256` now reads:

```
# status: anchored
# anchor: d75d246789f628d19e8821a41a9881d151c7a6fa
# reviewed: 2026-11-09
67a7d4ef…  crates/ic-stable-lara/src/slab_index.rs
5f352ac2…  crates/ic-stable-lara/src/labeled/bucket_label_key.rs
```

Checker transcript:

```text
$ python3 scripts/check-lean-sources.py
fixture crates/ic-stable-lara/formal/SOURCES.sha256 [anchored anchor d75d2467…]
  ok: 2 source(s) unchanged
fixture crates/ic-stable-linear-hash-map/formal/SOURCES.sha256 [pending-reanchor anchor 0da342d6…]
  ADVISORY changed: …/src/control.rs (re-anchor before flipping to anchored)
  ADVISORY changed: …/src/map.rs (re-anchor before flipping to anchored)
fixture crates/ic-stable-vec-deque/formal/SOURCES.sha256 [obsolete …]
  skipped: fixture models deleted code (see its SCOPE.md)
summary: 3 manifest(s), 0 blocking, 2 advisory/anchored-clean, 1 obsolete
```

The lara fixture therefore exercises the blocking path from today: any future change to the two
cited sources turns the checker red until the model is re-read and the hashes are refreshed together.

## Documentation updates

- `formal/SCOPE.md`: anchor timestamp and target revision now `d75d2467`; the freshness section says
  `anchored` and blocking, with the re-anchor procedure.
- `formal/REPORT.md`: dated re-anchor section stating the rename-only change and the same axiom set.
- ADR 0104: implementation note recording lara as anchored/blocking (lhm pending, vec-deque obsolete).

## Remaining work (later slices)

1. Re-anchor lhm `control.rs`/`map.rs` (the P3–P5 question in the classification record), then flip
   its manifest to `anchored`.
2. Restate the vec-deque model for the ADR 0086 segmented ring and give it a fresh manifest.
3. Model the new lara exclusive-end/validity helpers, or retire the fixture when the Verus migration
   criteria are met (ADR 0104).
