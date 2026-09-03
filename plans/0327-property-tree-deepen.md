---
name: "property-tree deepen — replace the per-w PropertyTreeRootCapacityReached fail-closed with a right-spine cascade, lifting the effective w > 0 slot cap to TREE_STRUCTURAL_CAP = 2^30"
overview: "The depth-1 property tree (property root = ceil(S/K) LPB leaf ids at edge_start + edge_root_len) fail-closes at R_MAX = 1024 entries. Wire the property-tree cascade: (1) store the property-tree physical depth packed into the tree-mode depth byte (bits 0-1 edge, bits 2-3 property — derived d' is unsound under tombstone compaction shrinkage); (2) depth-generic property resolver (leaf radix K, interior radix B); (3) property-tree deepen packing the root into ceil(root_len/B) InlinePropertyInterior blocks with a combined-span realloc [edge root | new property root], LIFO rollback + publish-before-release; (4) rewire all growth sites (insert tail-room, tail_append_depth1, tail_append_depth_ge2 — which also fixes its latent w > 0 silent-corruption gap, tree_mode_deepen's depth-aware anticipation) and promote (build d'-correct tree instead of fail-closing); (5) property_root_region_len becomes the d'-aware single source of truth; (6) PropertyTreeRootCapacityReached remains only as the d' = 3 backstop, which coincides with the 2^30 structural cap. MAX_PROPERTY_DEPTH = 3 (K × R_MAX × B² ≥ 2^30 for every w ≥ 1)."
todos:
  - id: "audit-depth-representation"
    content: "Audit resolver call sites, property-root length computation sites (compact/tail-append/deepen/promote/demote), and decide the depth representation. Outcome: stored packed depth byte (derived rejected — removal/compaction shrinkage breaks it without an eager flatten; handoff escalation answered by packed byte). Coverage math: d' ≤ 3 covers all w at 2^30."
    status: completed
  - id: "packed-depth-byte"
    content: "record.rs: pack edge depth-1 (bits 0-1) and property depth-1 (bits 2-3) into the repurposed inline_property_bytes_log_len byte; new accessors tree_mode_property_depth / with_tree_mode_property_depth; validation bounds (tree: byte ≤ 10, low bits ≤ 2; w == 0: byte ≤ 2) in try_from_parts + try_read_from; demote paths verified to construct fresh zero-byte slab rows."
    status: completed
  - id: "depth-generic-resolver"
    content: "tree_read.rs: resolve_property_leaf_block_id descends the B-radix interior chain from the derived property root offset (edge_start + bucket_span_region_len); read/write_property_value_at_slot unchanged above it; compact.rs property_root_region_len becomes ceil(L_p / B^(d'-1))."
    status: completed
  - id: "property-cascade"
    content: "tree_write.rs: helpers property_root_entry_count / property_depth_for_leaves (typed 2^30 backstop) / tree_mode_property_deepen (LIFO rollback, publish-before-release, debug_assert vs property_root_region_len) / ensure_property_tree_depth (re-reads the canonical descriptor) / plan_property_leaf_append + release_property_plan; rewire tree_mode_insert_edge tail-room, tree_mode_tail_append_depth1, tree_mode_tail_append_depth_ge2 (combined-span realloc in both branches — fixes the latent w > 0 bug), tree_mode_deepen (depth-aware anticipation, pre-deepen when the anticipated mint would overflow); promote.rs builds the d'-correct tree instead of fail-closing."
    status: completed
  - id: "tests-and-validation"
    content: "5 new lib tests (w=32 K-boundary full parity over 131,073 slots with decoy span, 200-slot LCG model test, w=1..4096 boundary math, w=4 edge-deepen × property-deepen anticipation, w=1 depth_ge2 both branches); retarget lpb_in_tree_rework_f2_cap_guard to the 2^30 backstop. Green bar: check / lib 546 (baseline 541) / canbench lib 490 / clippy -D warnings / fmt --check. Wasm: 4,265,392 → 4,299,187 bytes, exports 286 unchanged, name chars 39,036 → 39,099. ADR 0088 §2/§4 amended. validate_plan.py --phase final."
    status: completed
isProject: false
---

# Plan 0327 — ic-stable-lara property-tree deepen (per-w cap lift to 2^30)

## Objective

Replace the depth-1 property-tree fail-closed guard
(`PropertyTreeRootCapacityReached` at property root len = `R_MAX = 1024`)
with a right-spine cascade, lifting the effective per-`w` slot cap of
`w > 0` tree-mode buckets to the edge tree's `TREE_STRUCTURAL_CAP = 2^30`.

## Context

- Handoff: /tmp/handoff-property-tree-deepen.md. Baseline HEAD `5cd4da80c`
  (fmt follow-up on the tree_csr rename). Baseline: 541/490 lib tests,
  wasm 4,265,392 bytes, exports 286.
- Tree mode (ADR 0088): edges in the LTB block tree (Plan 0325 cascade,
  cap 2^30), property values in LPB blocks (Plan 0326 LPB-in-tree,
  depth-1 only). Property root = ceil(S/K) leaf ids at
  `edge_start + edge_root_len` (derived; gap-0 combined span).
- Layout contract (inviolable): property root offset is derived, never
  stored; every write path must produce region lengths matching
  `compact::property_root_region_len`.

## Scope

- IN: packed depth byte (record.rs), depth-generic property resolver
  (tree_read.rs), property-tree deepen cascade + growth-site rewiring
  (tree_write.rs), promote d'-correct tree build (promote.rs),
  d'-aware property_root_region_len (compact.rs), guard test
  retargeting, 5 new unit tests, ADR 0088 amendment.
- OUT (unchanged fail-closed): 0→w on existing tree buckets, w1→w2
  re-encoding, w→0 teardown, batch `commit_with_location_mode` tree
  admission for `w > 0`.

## Expected Change Surface

| File | Change |
|---|---|
| `crates/ic-stable-lara/src/labeled/record.rs` | Packed depth byte: accessors + validation bounds |
| `crates/ic-stable-lara/src/labeled/graph/tree_read.rs` | Depth-generic `resolve_property_leaf_block_id`; guard test retarget |
| `crates/ic-stable-lara/src/labeled/graph/tree_write.rs` | Cascade helpers + 4 growth-site rewires + depth_ge2 w>0 latent bug fix + tail-room span release |
| `crates/ic-stable-lara/src/labeled/graph/promote.rs` | F-2 fail-close → d'-correct tree build |
| `crates/ic-stable-lara/src/labeled/graph/compact.rs` | d'-aware `property_root_region_len` |
| `design/adr/0088-tree-csr-mode-for-high-degree-label-buckets.md` | §2 cap / §4 depth-representation amendment, Plan 0327 entry |

## Steps

1. Audit (done, recorded in Audit decisions below).
2. record.rs packed depth byte.
3. tree_read.rs depth-generic resolver + compact.rs d'-aware root len.
4. tree_write.rs cascade helpers + growth-site rewiring; promote.rs.
5. Tests + green-bar + wasm metrics + ADR.

## Audit decisions

- **Depth representation: stored, packed.** The handoff recommended a
  derived property depth `d'(w, S)`, but the audit found derivation is
  unsound under tombstone compaction: `stored_slots` can shrink across
  a deepen boundary while the root stays deepened, so a derived `d'`
  under-walks the hop chain. Derived depth would require an eager
  property-tree flatten on every downward crossing with no hysteresis.
  Instead the tree-mode depth byte (`inline_property_bytes_log_len`
  repurposed) now packs both depths: bits 0-1 = edge physical depth − 1,
  bits 2-3 = property physical depth − 1 (byte ≤ 10; `w == 0` tree
  buckets still ≤ 2). All access goes through
  `LabelBucket::tree_mode_physical_depth` / `tree_mode_property_depth`.
- **MAX_PROPERTY_DEPTH = 3.** Coverage at depth `d'` is
  `K × R_MAX × B^(d'-1)`; `d' = 2` suffices only for `K ≥ 1024`
  (`w ≤ 4`), `d' = 3` covers all `w ≥ 1` up to `2^30` and beyond.
- **Property root region length** at depth `d'`:
  `ceil(L_p / B^(d'-1))` with `L_p = ceil(S / K)`,
  `K = floor(4096 / w)` — single source of truth in
  `compact::property_root_region_len` (write paths debug-assert
  equality).
- **Latent bug found and fixed:** `tree_mode_tail_append_depth_ge2`
  ignored `w > 0` entirely — no K-boundary property leaf mint and an
  edge-root-only realloc that would orphan the property root
  (reachable today for `w ≤ 3` where `K > 1024`, e.g. w=1 past 2^20).
- **Pre-existing span leak fixed:** the tail-room branch of
  `tree_mode_insert_edge` did not release the old combined span after
  publish (the depth-1 append path did).
- **Interior block levels:** `BlockKind::InlinePropertyInterior` header
  level = height above leaves (1 = holds LPB leaves, 2 = holds level-1
  interiors).

## Completion Criteria

- [x] Property-tree deepen fires at the K × R_MAX boundary and all
      property values remain readable through the production reader.
      (`property_deepen_at_k_boundary_preserves_all_values` — w=32
      full parity over 131,073 slots after deepen, decoy span to
      defeat allocator adjacency.)
- [x] Random-slot read/write after deepen matches an in-memory model.
      (`property_deepen_random_slot_read_write_model` — 200 LCG slots.)
- [x] Boundary math: `property_root_entry_count` /
      `property_depth_for_leaves` verified for all w in 1..=4096 at
      the 2^30 cap; typed backstop fires past it.
      (`property_root_entry_count_and_depth_boundary_math`.)
- [x] Edge deepen × property deepen order compatibility.
      (`edge_deepen_with_property_deepen_anticipation` — w=4, both
      trees deepen in one insert.)
- [x] depth_ge2 property append works in both branches.
      (`depth_ge2_property_append_at_k_boundary` — w=1.)
- [x] Guard test retargeted; `PropertyTreeRootCapacityReached` fires
      only at the 2^30 backstop. (`lpb_in_tree_rework_f2_cap_guard`.)
- [x] Green-bar all green; wasm metrics recorded; ADR 0088 amended.
      (check clean; lib 546 / 0 failed (baseline 541); canbench lib
      490 / 0 failed (baseline 490); clippy `-D warnings` clean;
      `cargo fmt --check` clean; wasm 4,299,187 bytes (+33,795),
      exports 286 unchanged, export name chars 39,099 (+63);
      ADR 0088 §2/§4 amended with the Plan 0327 entry.)

## Validation

- `cargo check -p ic-stable-lara` — clean.
- `cargo test -p ic-stable-lara --lib --no-default-features` — 546 pass (baseline 541).
- `cargo test -p ic-stable-lara --lib --features canbench` — 490 pass (baseline 490).
- `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings` — clean.
- `cargo fmt --check -p ic-stable-lara` — clean.
- wasm (canbench build): 4,265,392 → 4,299,187 bytes (+33,795); exports 286 unchanged; export name chars 39,036 → 39,099 (+63; no new exports).

## Later Slices (recorded, not in this plan)

- canbench bench for the property-root-full deepen boundary (seeding
  cost ~2.2G ins at w=32 fits the 10T limit) — optional per handoff,
  deferred to keep this slice reviewable; yml untouched.
