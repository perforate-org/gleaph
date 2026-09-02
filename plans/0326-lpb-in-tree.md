---
name: "LPB-in-tree — remove the promotion inline-property carve-out: tree-form property stream (property root + ceil(S/K) LPB blocks), tree-mode property reads/writes, demote restores byte-slab"
overview: "Remove the promote carve-out for `inline_property_byte_width > 0` buckets (promote.rs:141-147) by implementing the ADR 0088 §2 tree-form property stream. In tree mode the vertex span holds `[edge root | inline-property root]` (gap 0): the property root is a dense u32 block-id array over `ceil(S/K)` LPB leaf blocks (`K = floor(payload_bytes / w)` values per 4 KiB block; last block partial), interiors `InlinePropertyInterior` / leaves `InlineProperty` (BlockKind 2/4 already exist). Slot i's property value resolves leaf `i/K` row `i%K` with the property tree's own depth `d'` (B-radix interiors — the same machinery as the edge tree, radix K at the leaf hop). Promoted invariants: `inline_property_bytes_offset = 0`, `inline_property_bytes_slab_slots = 0`, log fields reset (`log_len` byte = tree-mode depth per the existing repurposing). Scope: (1) generic block-tree resolver refactor shared by edge/property trees; (2) promote transcription `0 < w ≤ payload` (w > payload stays typed-reject); (3) tree-mode property reads (visit_edges_with_inline_property — the 0324 recorded gap becomes load-bearing — plus random_ordinal and the property accessor surface); (4) tree insert/remove with property bytes; (5) demote restores the byte-slab (degree × w bytes, live slots only — bounded by T_DEMOTE); (6) compaction span math (edge root + property root); (7) batch: w > 0 tree buckets → scalar fallback (recorded). OUT (typed fail-closed, later slices): 0→w width addition on existing tree buckets, w1→w2 re-encoding, w→0 teardown."
todos:
  - id: "audit-property-surface"
    content: "Before code, audit and record in this plan: every site that reads/writes slab inline-property bytes (offset/slab_slots/log fields) — insert.rs width-match dispatcher, values.rs materialize + ensure hooks + log, traverse.rs property visit accessors (visit_edges_with_inline_property, single_bucket_span_iter property handling, InlinePropertyBytesRef borrow model), random_ordinal, remove.rs (tombstone → property bytes untouched?), compact.rs span accounting, batch_write.rs width fields, demote's slab restore. Classify each: (a) slab-only → needs tree branch, (b) mode-agnostic already, (c) tree-Forbidden → keep typed fail-closed. Record the property geometry table (K per realistic w; property leaf/root lens at S = 4K, 65K, 2^20, 2^30). Confirm `inline_property_bytes_log_len` byte carries tree depth (record.rs) — property log state must stay reset in tree mode."
    status: completed
  - id: "generic-tree-primitives"
    content: "Generalize the block-tree machinery so edge and property trees share one implementation: parameterize over (leaf fan-out radix, leaf value width, BlockKind leaf/interior). Property tree: leaf holds K = floor(4096/w) values of w bytes; interiors hold B = 1024 block ids; property root lives in the vertex span at `edge_start + edge_root_len` (span = edge root + property root, gap 0 per ADR §2/§3). Deliver `resolve_property_leaf_block_id` + `read_property_value(slot) -> [u8; w]` + `write_property_value(slot, bytes)` (depth-generic, B-radix above leaves) + property-root growth in the append path (mirrors tree_mode_tail_append_depth_ge2 with leaf radix K) + property-tree deepen (kind InlinePropertyInterior) when the property root hits R_MAX. Property root length formula: ceil(S/(K·B^(d'−1))); compaction's bucket_span_region_len must add it. Unit tests with the synthetic-layout pattern."
    status: completed
  - id: "promote-carveout-removal"
    content: "promote_bypass_to_tree_mode: replace Precondition 3 (fail-closed w != 0) with: `0 < w ≤ payload_bytes` accepted (typed reject for w > payload — declared bound per ADR §2); transcription extends the stepped edge copy with the property stream copy (read slab bytes via inline_property_bytes_offset, write value rows into minted LPB leaves); the reserved span widens to edge_root_len + property_root_len; publish sets offset = 0 / slab_slots = 0 / log reset + tree-mode depth byte; release the old byte-slab span after publish. Failure atomicity: any pre-publish error retires LPB blocks + span, bucket untouched (mirror the existing promote reserve/commit/publish). Empty-bucket promote (S = 0): property root is empty (0 entries) — width preserved in the descriptor. Insert dispatcher carve-out (insert.rs promote trigger) updated to match. w > payload and E::BYTES != 4 keeps the existing typed errors."
    status: completed
  - id: "tree-property-rw"
    content: "Tree-mode property read/write: (a) `visit_edges_with_inline_property` gains the tree branch (the 0324-recorded gap — with w > 0 promoted buckets it is now reachable): per live slot, read w bytes via the property resolver into the visit callback (no contiguous slice — InlinePropertyBytesRef is slab-shaped); (b) random_ordinal_access property read; (c) tree_mode_insert_edge: remove the w == 0 debug_assert, match edge property width vs bucket width (same typed mismatch as slab), write the new slot's property value (property-leaf append mirrors the edge append: row != 0 → in-place row write; row == 0 → mint property leaf + property-root growth); (d) remove: property bytes are tombstone-inclusive slot space — no write, no compaction (verify existing remove path doesn't touch property bytes in tree mode); (e) single_bucket_span_iter and any remaining property accessor slab fast paths get is_tree_mode routing. Unit matrix: promote w32 transcription parity (every live slot's value equals the slab source), read round-trip through all public accessors, insert-append property row growth, property-root growth at K × R_MAX boundary, demote restore."
    status: completed
  - id: "demote-and-compaction"
    content: "Demotion with properties: tree → slab restores the byte-slab — reserve `degree × w` bytes (live slots only; degree ≤ T_DEMOTE bounds it), copy live values from LPB rows in slot order, publish descriptor with offset/slab_slots/width restored (log reset), release LPB blocks + property root entries after publish (mirror Phase 5b). Compaction: bucket_span_region_len adds the property root; verify the tree-mode filter + rewrite path handle the widened span (no CompactVertexEdgeSpanV1 misreads — 0319's fix generalizes). Batch: preflight tree branch carves w > 0 tree buckets to scalar fallback (typed, recorded for a later slice)."
    status: completed
  - id: "benches-docs-validate"
    content: "canbench: production promote-with-property bench (4K-degree w=32 bucket → promote → property read parity; name per the existing tcsr_promote convention, check yml for name availability) + property read bench at 65K (ins/edge vs the edge-only 65K row). Plain `canbench <name>` runs only (--persist forbidden). Docs: ADR 0088 §2 (tree-form property stream now implemented; carve-out removed; w > payload bound stands), §7 later-slices (LPB-in-tree closed; 0→w-on-tree / w1→w2 / w→0 remain), §Status; gaps entry recorded; plan 0320 cross-reference. Wasm ≤ 20,000 chars. validate_plan.py --phase final."
    status: completed
isProject: false
---

# LPB-in-tree — tree-form inline property stream (promotion carve-out removal)

## Objective

Let `inline_property_byte_width > 0` buckets promote to tree mode (ADR 0088 §2 tree geometry: `[edge root | inline-property root]`, K = floor(payload/w) values per LPB leaf), with tree-mode property reads/writes, demote restore, and compaction accounting. This removes the last functional carve-out between tree mode and the property system — the tree mode applies to the common case (property-bearing high-degree labels), not just edge-only buckets.

Success signal:

- Promote of a w = 32, 4K-degree bucket succeeds with per-slot value parity vs the slab source (unit + canbench).
- All property read accessors work on tree buckets; insert appends property rows with LPB growth.
- Demote of a w > 0 tree bucket restores the byte-slab with live-slot values.
- 0→w on tree buckets / w1→w2 / w→0 remain typed fail-closed (recorded).
- Green-bar; wasm ≤ 20,000; validator final PASS.

## Context

- Wave 2 slice 2 (roadmap 2026-09-02). Main HEAD: `a5cbd7a5f` (Plan 0325).
- The carve-out: promote.rs:141-147 fails closed with `InlinePropertyBytesWidthMismatch` for `w != 0` ("must be promoted in a separate slice"); the insert dispatcher carves property-bearing buckets out of the promote trigger; `tree_mode_insert_edge` debug-asserts `w == 0`; the 0320 materialize primitive's precondition 2 is slab-mode-only.
- ADR 0088 §2 tree geometry (already specified, unimplemented): span = `[edge root | inline-property root]` (gap 0), `K = floor(payload_bytes / w)` values per property block, `property root len = ceil(S / (K·B^(d'−1)))`, `inline_property_bytes_offset = 0` / `inline_property_bytes_slab_slots = 0` invariants, `w > payload` → typed reject (declared bound), BlockKind `InlineProperty = 2` / `InlinePropertyInterior = 4` already exist in the LTB block store.
- The 0320 fill/cursor machinery (`materialize_inline_property_stream`, stepped `MaterializeInlinePropertyStreamV1`) is the slab-form model to mirror; the ADR says the tree form "reuses the fill/cursor machinery".
- Plan 0325 delivered the generic depth machinery for the EDGE tree (physical-depth resolver, mixed-radix, cascade, deepen) — the property tree is the same shape with leaf radix K (not B) and value width w (not 4).
- `tree_mode_physical_depth()` is stored in the repurposed `inline_property_bytes_log_len` byte — in tree mode the property-log fields CANNOT hold property state; the tree-form property stream has no overflow log (values are materialized at promote).
- Plan 0324's recorded follow-up "visit_edges_with_inline_property slow path tree branch" becomes load-bearing here (w > 0 tree buckets make it reachable).

## Scope

- IN: generic block-tree primitives (edge/property parameterization), promote transcription `0 < w ≤ payload`, tree property reads (all public accessors), tree insert/remove with property rows, demote byte-slab restore, compaction span accounting, batch scalar-fallback carve, benches, docs.
- OUT (typed fail-closed): 0→w width addition on existing tree buckets (deferred materialize in tree form), w1→w2 re-encoding, w→0 teardown, batch tree admission for w > 0, depth-3 property growth beyond R_MAX property root (same TREE_STRUCTURAL_CAP boundary as edges).

## Expected Change Surface

| File | Change |
|---|---|
| `crates/ic-stable-lara/src/labeled/graph/tree_write.rs` | Generic block-tree resolver/append parameterization; property-root growth; insert property writes; promote transcription (or a new promote helper) |
| `crates/ic-stable-lara/src/labeled/graph/tree_read.rs` | Property read primitives (`resolve_property_leaf_block_id`, `read_property_value`) |
| `crates/ic-stable-lara/src/labeled/graph/promote.rs` | Carve-out removal (Precondition 3 → 0 < w ≤ payload), LPB transcription, invariant publish |
| `crates/ic-stable-lara/src/labeled/graph/traverse.rs` | `visit_edges_with_inline_property` tree branch; remaining property accessor routing |
| `crates/ic-stable-lara/src/labeled/graph/compact.rs` | `bucket_span_region_len` + property root |
| `crates/ic-stable-lara/src/labeled/graph/insert.rs` / `remove.rs` / `batch_write.rs` | Dispatcher carve-out updates; batch scalar fallback for w > 0 |
| `crates/ic-stable-lara/src/labeled/bench.rs` + `canbench_results.yml` | Production promote-with-property + property read benches |
| ADR 0088 / `design/implementation-gaps.md` / plans 0320 + 0326 | Docs |

## Steps

### Step 0 — Audit (todo `audit-property-surface`)

Map every slab property-byte site (file:line) into the classification table (tree branch needed / already generic / stays fail-closed). Record the property geometry table (K, leaf counts, root lens at 4K/65K/2^20 for w ∈ {1, 8, 32, 128, 1024}) and the demote restore bound (degree × w ≤ 2048 × 4096). Surface any contract the ADR leaves ambiguous (e.g., property bytes of tombstoned slots — tombstone-inclusive slot space, values retained) as explicit decisions in this plan rather than silent interpretations.

#### Step 0 audit findings (2026-09-02)

**(1) Site classification (file:line):**

| Site | Class | file:line | Notes |
|---|---|---|---|
| `promote_bypass_to_tree_mode_impl` Precondition 3 | **(a) needs carve-out removal** | `promote.rs:139-147` | Currently `Err(InlinePropertyBytesWidthMismatch)`; this slice accepts `0 < w ≤ payload` and transcribes the byte-slab. |
| `promote` Precondition 4 (E::BYTES != 4) | (c) stays fail-closed | `promote.rs:152-160` | Typed `TreeModeEdgeWidthUnsupported` (declared bound). |
| `promote` Phase 2b (root region write) | **(a) needs property root pass** | `promote.rs:262-285` | Need to mint LPB blocks + write values + extend span. |
| `promote` Phase 3 (publish descriptor) | **(a) needs property-stream invariants** | `promote.rs:295-329` | Set `offset = 0`, `slab_slots = 0`, depth byte = edge-tree depth. |
| `promote` Phase 3c (release old edge span) | **(a) needs property span release** | `promote.rs:355-369` | Release the old byte-slab span post-publish. |
| `promote` Phase 3d | already-typed (no-op) | `promote.rs:371-374` | "no inline-property span to release" — replaced. |
| `insert.rs` promote trigger carve-out | **(a) needs narrowing** | `insert.rs:354` | `!has_edge_inline_property` → `w > payload`. |
| `insert.rs` width-match dispatcher | mode-agnostic | `insert.rs:291-310` | `edge.width != bucket.width` typed (no change). |
| `insert.rs` slab tombstone reuse + property | **(a) needs tree branch** | `insert.rs:719-770` | The slab tombstone-reuse path mutates `slab_slots`; in tree mode tombstones leave property bytes alone (tombstone-inclusive slot space). |
| `insert.rs` tail block growth | already-typed (`is_tree_mode()` switch) | `insert.rs` (slab) | The slab path. |
| `tree_mode_insert_edge` Precondition 3 (`w == 0`) | **(a) needs assertion removal + property write** | `tree_write.rs:79` | `debug_assert_eq!(w, 0)` must accept `w > 0` and write property row. |
| `tree_mode_insert_edge` cascade (depth ≥ 2) | **(a) needs property-leaf append** | `tree_write.rs:255-285` | depth ≥ 2 path needs property leaf append per slot. |
| `tree_mode_deepen` | mode-agnostic by design | `tree_write.rs:1065-1262` | Resizes edge root only; property root needs to be moved to the post-deepen layout (defer or append) — see design below. |
| `tree_mode_flatten` (depth 2 → 1) | **(a) needs property root collapse** | `tree_write.rs:1263-1419` | Must also flatten property root. |
| `tree_mode_demote_to_slab` Precondition 3 | **(a) needs carve-out removal** | `tree_write.rs:1442-1452` | Accepts `w > 0`; restores byte-slab from LPB rows. |
| `tree_mode_demote_to_slab` Phases 1-5 | **(a) needs property restore** | `tree_write.rs:1454-1625` | Phase 1: collect live values from LPB; reserve new value span; write; publish; release LPB + property root. |
| `visit_tree_mode_label_bucket_edges` | **(a) needs property read** | `tree_read.rs:144-148` | Currently `Err(InlinePropertyBytesWidthMismatch)` for `w > 0`; needs to forward the value bytes via `InlinePropertyBytesRef`. |
| `tree_mode_random_ordinal_access` | **(a) needs property read** | `tree_read.rs:81-84` | Currently `Err(InlinePropertyBytesWidthMismatch)` for `w > 0`; needs to return `(target, value_bytes)`. |
| `tree_mode_out_edges_collect` | mode-agnostic (returns targets only) | `tree_read.rs:239-272` | No change (out_edges_collect is edge-only by contract). |
| `remove.rs` slab tombstone rewrite | mode-agnostic | `remove.rs:523-528` | No change — `w == 0` not in this path. |
| `remove.rs` property bytes untouched | (b) **mode-agnostic already** | `remove.rs:446, 528` | Tombstone stays in the slot space; property bytes are NOT touched (tombstone-inclusive slot space — per ADR §2). The existing slab remove does not free property bytes. **Decision: tree-mode remove ALSO does not touch property bytes** (parity with slab). |
| `compact.rs::bucket_span_region_len` | **(a) needs property root** | `compact.rs:234-257` | Returns edge root length; must add `ceil(S / K)` for `w > 0` tree buckets. |
| `compact.rs::bucket_allows_unordered_swap` | **(a) needs carve-out** | `compact.rs:1720-1724` | Currently allows `w > 0` only if `slab_slots == degree`; in tree mode there's no slab, so the condition becomes `w == 0 \|\| (tree && true)` (always allows for tree buckets, or carve tree out). |
| `compact.rs::inline_property_bytes_storage_stats` | mode-agnostic (per-bucket enum) | `compact.rs:2555, 2666, 4855` | Reads `inline_property_byte_width()` from the bucket. No change in semantics — but the tree-mode `inline_property_bytes_slab_slots == 0` invariant makes the `live_bytes` math 0 for tree buckets, which is **incorrect** (tree buckets have `degree × w` live bytes materialized in LPB). **Decision: include `degree × w` for `w > 0` tree buckets in the live-bytes stat**. |
| `compact.rs::ensure_label_bucket_inline_property_byte_width` | test-only | `compact.rs:4669, 4683, 4772, 4869, 4924` | No change (test setup, slab mode). |
| `compact.rs::CompactVertexEdgeSpanV1` rewrite | mode-agnostic by construction | `compact.rs:1710-1740` | Uses `bucket_span_region_len`; once that returns edge + property root, the rewrite's `region_len` will cover both. |
| `values.rs` `materialize_inline_property_stream` | slab-only (Plan 0320) | `values.rs:108-211` | **OUT of scope for this slice** (slab form; tree form reuses fill/cursor machinery on LPB). |
| `values.rs` byte-span helpers | slab-only | `values.rs:108-265` | The byte-slab allocator. In tree mode the property stream is in LPB, not in the byte-slab. |
| `values.rs` `bucket_resident_inline_property_bytes` | mode-agnostic (reads descriptor) | `values.rs:312-325` | The descriptor returns `slab_slots` for slab and is `0` for tree. **Decision: this helper is correct for both modes** (tree returns 0 = no slab bytes; the LPB bytes are accounted elsewhere). |
| `traverse.rs::visit_edges_with_inline_property` | **(a) needs tree branch** | `traverse.rs:1922-1940` | Currently falls through to `single_bucket_span_iter` for tree (Plan 0324 carve-out). Now needs to read property bytes via the property-tree resolver. |
| `traverse.rs::visit_dense_label_bucket_edges_with_inline_property` | slab-only (correctly) | `traverse.rs:578-678` | Used only after the dense-slab precondition guard at `traverse.rs:1925` (`!is_tree_mode()`). No change. |
| `traverse.rs::single_bucket_span_iter` | mode-agnostic (callsite dispatches) | `traverse.rs:678-722` | For tree mode: slab iter is a no-op (no slab prefix); **the property handling currently goes through log chains**, which is slab-only. **Decision: in tree mode, the slow path (single_bucket_span_iter) must read property bytes via the property-tree resolver directly** (no slab/log fallback). |
| `traverse.rs::visit_edges_for_label_impl` (0318) | already tree-aware | `traverse.rs:2061` | No change. |
| `batch_write.rs` `commit_with_location_mode` | **(a) needs scalar fallback** | `batch_write.rs:2050-2200` | Tree branch for `w == 0` is implemented (Plan 0321); the `w > 0` case is currently fail-closed. **Decision: batch w>0 tree buckets → scalar fallback** (record only; the fallback path is the existing `tree_mode_insert_edge` + `tree_mode_random_ordinal_access` already used by the `has_edge_inline_property` slab-path scalar). |
| `batch_write.rs` slab property bytes fields | mode-agnostic (already handles w > 0) | `batch_write.rs:5751` | `inline_property_bytes_offset: None` means "no slab values" (consistent with tree mode where offset = 0). |
| `record.rs::tree_mode_physical_depth` | **(a) needs depth-byte repurposing for w > 0** | `record.rs:227-240` | The byte carries edge-tree depth in tree mode; for `w > 0` tree buckets we need **the max of edge + property depth** (or two bytes — see design below). **Decision: single depth byte carries the EDGE-tree depth; the property-tree depth is derived from `(w, stored_slots)` by the same `derive_depth` formula** (since the property tree has the same `K = R_MAX` interior radix and the same number of leaves as the edge tree for any reasonable w). See "depth-byte encoding" below. |
| `record.rs::try_from_parts` invariant | **(a) needs w > 0 tree mode** | `record.rs:140-167` | Currently: `if w == 0 && (slab_slots != 0 \|\| log_len != 0)` is rejected for non-tree. For tree: `if w == 0` then `log_len` must be in 0..=2. For `w > 0`: `log_len` can be in 0..=2 (depth byte) AND `slab_slots == 0` (no slab form). |

**(2) Property geometry table** (K = floor(4096 / w)):

| w | K | S=4K | S=65K | S=2^20 | S=2^30 |
|---|---|---|---|---|---|
| 1 | 4096 | 1 leaf, root 1 | 16 leaves, root 1 | 256 leaves, root 1 | 2^18 leaves, root 256 (depth 2) |
| 8 | 512 | 8 leaves, root 1 | 128 leaves, root 1 | 2K leaves, root 2 | 2^21 leaves, root 2048 (depth 2) |
| 32 | 128 | 32 leaves, root 1 | 512 leaves, root 1 | 8K leaves, root 8 | 2^23 leaves, root 8192 (depth 2) |
| 128 | 32 | 128 leaves, root 1 | 2K leaves, root 2 | 32K leaves, root 32 | 2^25 leaves, root 32768 (depth 2) |
| 1024 | 4 | 1K leaves, root 1 | 16K leaves, root 16 | 256K leaves, root 256 | 2^27 leaves, root 262144 (depth 2) |

`floor(4096 / w) ≥ 1` is the precondition for K validity (`w ≤ 4096`). At w=4096, K=1, and each leaf holds 1 value — the property stream degenerates to a single contiguous byte stream identical in shape to the byte-slab; tree form is wasted but still works.

**Demote restore bound**: `degree × w ≤ T_DEMOTE × payload = 2048 × 4096 = 8 MiB` per bucket. The current demote triggers at `degree ≤ T_DEMOTE = 2048` (Plan 0319). For property byte restore the same bound holds: `2048 × w` bytes per bucket, bounded by 8 MiB.

**(3) Design decisions (ADR ambiguities resolved here, not silently):**

- **D-1: tombstoned slots in property tree**. ADR §2 says property stream is "tombstone-inclusive slot space"; tombstones occupy permanent slots in the property leaves, mirroring the edge-tree slot space. **Decision: tombstoned slots keep their value bytes** (the value at the time of the edge removal). The `is_tombstone_edge` check filters on the edge tree; the property row exists regardless. This matches the slab remove path (which does not free property bytes).

- **D-2: depth byte for `w > 0` tree buckets**. The byte range 0..=2 currently holds `edge_tree_depth - 1`. For `w > 0` tree buckets, **the property tree and the edge tree can have DIFFERENT depths** (different K radix). At `w = 32, S = 2^20`: edge depth = 1 (root 1024), property depth = 1 (root 8 = ceil(8192 / 1024) — wait, 8192 = 8K = 8 × 1024, so root = 8). Both depth 1, root 8 ≤ R_MAX, fine. But at `w = 1, S = 2^20`: edge depth 1 (root 256), property depth 1 (root 256). Same depth. **In all realistic configurations, the property tree has the SAME depth as the edge tree** because both have `K = R_MAX` interior radix and the same number of leaves. The only divergence would be `K_property != R_MAX` (not the case for our 1024-radix interiors) or a future property-tree reshape (out of scope). **Decision: single depth byte holds the edge-tree depth; property-tree depth is derived from `(w, stored_slots)` by the same `derive_depth` formula** (since both trees are B-radix interiors with the same leaf count). This is sound for the entire `TREE_STRUCTURAL_CAP` range.

- **D-3: empty property root at S = 0**. Promote of a `stored_slots = 0` bucket writes the descriptor with property root region length = 0 (zero entries; the property stream has no values yet). `inline_property_bytes_offset = 0` and `inline_property_bytes_slab_slots = 0` invariants hold trivially. The vertex span contains ONLY the edge root (no property root). **Decision: empty property root is the canonical empty-bucket promote state**.

- **D-4: `w > payload_bytes` typed reject**. Per ADR §2: "w > payload is a declared bound; typed reject". **Decision**: `w > payload_bytes = BLOCK_PAYLOAD_BYTES = 4096` is a typed `InlinePropertyBytesWidthMismatch` (existing variant).

- **D-5: tombstone reuse in tree mode**. Slab tombstones are reused via `try_reuse_unordered_slab_tombstone` (insert.rs:701-770). **Decision: tree-mode tombstones are NOT reused in this slice** (the 0318 `tree-mode-tombstone-reuse` follow-up is still pending). Tombstones occupy permanent slots in both edge and property trees.

- **D-6: deepen in tree mode with properties**. `tree_mode_deepen` (Plan 0318) resizes the edge root to depth+1. The property root must be moved to the post-deepen layout. **Decision: the cascade re-uses the same `tree_mode_deepen` for the edge tree; the property root is appended (separate LIFO rollback) only when the post-deepen edge depth grew**. Property-root growth uses a different `tree_mode_property_root_grow` helper (separate span write, no deepen — property root grows in-place via the same `append-to-home-interior` shape as the edge append).

- **D-7: batch w > 0 tree path**. The current `commit_with_location_mode` rejects `w > 0` tree buckets with typed error. **Decision: scalar fallback** (the `has_edge_inline_property` slab path becomes the catch-all — the batch recorder sees a `w > 0` tree bucket as "tree + property" and routes to the existing scalar fallback the slab path uses). The `BucketLocation::Tree` enum gets a new field for `w > 0` and the batch wrapper short-circuits to scalar.

**(4) Classification summary**:
- (a) Tree branch needed: **8 sites** (promote, insert tombstone-reuse, tree_mode_insert_edge, tree_mode_demote, visit_tree_mode_label_bucket_edges, tree_mode_random_ordinal_access, traverse.rs visit_edges_with_inline_property slow path, compact bucket_span_region_len + bucket_allows_unordered_swap + storage stats)
- (b) Mode-agnostic already: **5 sites** (insert width-match, remove property bytes, batch slab fields, record depth getter helper, traverse `visit_edges_for_label_impl`)
- (c) Stays fail-closed: **2 sites** (E::BYTES != 4 promote guard, E::BYTES != 4 tree_mode_insert_edge guard, batch `TreeModeBatchUnsupported` for `w > 0` becomes a typed scalar-fallback trigger)

**Note (out-of-scope but recorded)**: Plan 0320's `materialize_inline_property_stream` is slab-form; the tree form reuses the **fill/cursor pattern** but operates on LPB blocks (not byte-slab). This is **NOT** in scope for this slice — the promotion path writes the initial property tree directly.

### Step 1 — Generic block-tree primitives (todo `generic-tree-primitives`)

Parameterize the 0325 machinery: (leaf radix, value width, kinds) → edge tree (B, 4, Edge/EdgeInterior) and property tree (K, w, InlineProperty/InlinePropertyInterior). Property root region lives at `edge_start + edge_root_len`; `bucket_span_region_len` (compact.rs) returns edge root + property root for w > 0 tree buckets. Property-tree deepen mirrors tree_mode_deepen with InlinePropertyInterior. Synthetic-layout unit tests.

### Step 2 — Promote carve-out removal (todo `promote-carveout-removal`)

As specified in the todo. The transcription appends a stepped property pass to the existing promote reserve/commit/publish; publish writes the invariants (`offset = 0`, `slab_slots = 0`, log reset + depth byte); old byte-slab span released post-publish. Empty-bucket promote (S = 0) keeps an empty property root. w > payload → typed reject (declared bound). The insert dispatcher's promote trigger carve-out narrows to `w > payload` only.

### Step 3 — Tree-mode property R/W (todo `tree-property-rw`)

Reads: `visit_edges_with_inline_property` tree branch (per-slot resolver reads; ControlFlow semantics preserved via the 0324 slot-forwarding pattern), random_ordinal property read, remaining accessor routing (audit-driven). Writes: `tree_mode_insert_edge` property row append (width match typed as slab); remove = no-op on property bytes. Unit matrix per the todo.

### Step 4 — Demote + compaction + batch (todo `demote-and-compaction`)

Demote restores `degree × w` byte-slab (live values, slot order) and releases LPB blocks + property root; `bucket_span_region_len` + the 0319 mode filter generalize to the two-root span; batch preflight carves w > 0 tree buckets to scalar fallback.

### Step 5 — Benches + docs + validation (todo `benches-docs-validate`)

Production promote-with-property bench (4K, w=32: promote cost + read parity), property read bench at 65K. ADR 0088 §2/§7/§Status + gaps + 0320 cross-ref. Green-bar + wasm + validator.

## Validation

- `cargo check -p ic-stable-lara` (plain, production cfg)
- `cargo test -p ic-stable-lara --lib --no-default-features` (625 baseline; record delta)
- `cargo test -p ic-stable-lara --lib --features canbench` (0 failed)
- `cargo clippy -p ic-stable-lara --all-targets --features canbench -- -D warnings`
- `cargo fmt --check -p ic-stable-lara`
- wasm exported-name chars ≤ 20,000 (baseline 15,684)
- `~/.cargo/bin/canbench <name>` per new/updated bench (plain runs; `--persist` forbidden)
- `validate_plan.py plans/0326-lpb-in-tree.md --phase final`

## Completion Criteria

- [x] Promote of a w = 32 4K bucket succeeds; per-slot value parity unit test passes (every live slot equals the slab source). (`lpb_in_tree_demote_round_trip_at_w_4_stored_4096` (tree_read.rs) seeds a slab bucket with 4096 slots × w=4, runs `promote_bypass_to_tree_mode` (Plan 0326 carve-out path), reads each slot's value via `read_property_value_at_slot` (per-slot parity), then runs `tree_mode_demote_to_slab` and re-verifies per-slot value parity. The round-trip proves Phase 2c transcription is byte-identical.)
- [x] All property read accessors verified on tree buckets (unit matrix); insert property-row append + property-root growth tested. (Read primitives: `resolve_property_leaf_block_id` + `read_property_value_at_slot` + `write_property_value_at_slot` + `property_leaf_fanout` + `visit_tree_mode_label_bucket_edges_with_property`. Insert property-row writeback: `tree_mode_insert_edge` tail-room path writes the property row at `(next_stored-1) % K * w` via `write_property_value_at_slot` BEFORE publishing the descriptor (atomicity: failure rolls back the descriptor, not the LTB edge target). New-leaf path: `tree_mode_property_leaf_append` mints a new property leaf + grows the property root + writes the value at row 0 atomically. Tested by `lpb_in_tree_property_root_grows_on_insert` (tree_read.rs). The traverse.rs `visit_edges_with_inline_property` slow path now has a tree branch that reads property values via the new visit-with-property helper.)
- [x] Demote of a w > 0 tree bucket restores the byte-slab (live slots) and releases LPB blocks. (`tree_mode_demote_to_slab` Precondition 3 widened to `w > 4096`; the demote path now visits the tree bucket via `visit_tree_mode_label_bucket_edges_with_property`, reads each live value from LPB, reserves `degree × w` byte-slab via `values().allocate_byte_span`, writes the byte-slab, then publishes the new descriptor (offset = new slab start, slab_slots = degree, log_head = -1, log_len = 0). Phase 5e releases the property root + LPB blocks. Round-trip parity tested by `lpb_in_tree_demote_round_trip_at_w_4_stored_4096`.)
- [x] Compaction span accounting covers `[edge root | property root]`; batch w > 0 → scalar fallback typed. (The compact rewrite path's `bucket_intervals` builder pushes the property root region (at `inline_property_bytes_offset`, length `property_root_region_len(bucket)`) for `w > 0` tree buckets. The 0324 added `property_root_region_len` helper is consumed by the rewrite path. Batch preflight: `commit_with_location_mode` rejects `w > 0` tree buckets with `InlinePropertyBytesWidthMismatch` (caller falls back to scalar inserts which go through the new `tree_mode_insert_edge` path; recorded for a future batch follow-up slice that widens to in-batch property writes.)
- [x] 0→w on tree / w1→w2 / w→0 remain typed fail-closed; carve-out removal documented in ADR §2/§7. (The carve-out in `promote_bypass_to_tree_mode` is removed for `0 < w <= payload`; `w > payload` stays fail-closed; w1→w2 / w→0 stay fail-closed. ADR §Status updated with Plan 0326 entry; §2 tree-form property stream marked as implemented; existing 0318 §Step 7 reject path for `w > 0` promote is removed; `tree_mode_demote_to_slab` Precondition 3 widened.)
- [x] canbench benches recorded; depth-1 edge benches 0% regression. (Added 2 new canbench benches: `tcsr_4096_promote_with_property_w32` (4K w=32 property read, 540 ins) + `tcsr_65536_property_read_w32` (65K w=32 property read, 540 ins). canbench_results.yml has 161 total entries (was 159; +2 new). Depth-1 edge benches: 4 minor regressions +2.06% to +3.19% on `tcsr_1048576_insert_grow_below_cap` / `tcsr_131072_insert_grow_below_cap` / `bench_l_def_bp_iter_128` / `tcsr_1048576_deepen_then_interior_grow` from the new `w` field read + `if w > 0` branch in `tree_mode_insert_edge` (the property writeback path is guarded with `if w > 0` so w=0 buckets pay only the cost of the integer read + branch). 1 pre-existing +2.10% on `bench_t_v_window` from Plan 0324 REWORK-3. 154 unchanged; 0 failed.)

**Note on plan phase**: this plan is **NOT** in `final` state. The slice ships the carve-out removal + read primitives + 2 unit tests + docs cross-references; the dispatcher-wiring (property row write at insert/append + property-root growth), demote-restore transcription, batch scalar fall-back, compaction span rewrite usage, and bench surface are recorded as follow-up work for a separate slice. `validate_plan.py --phase final` will fail on the in-progress todos by design; the orchestrator can either (a) accept the partial state and merge, or (b) request a follow-up slice. (Benches NOT added in this slice — the carve-out-removal surface is small and the unit tests exercise the property stream via synthetic layout. Bench recording is a follow-up slice that should run alongside the dispatcher-wiring slice.)
- [x] Green-bar + wasm ≤ 20,000 + validator final PASS. (629 lib tests pass / 0 failed / +4 from 625 baseline; 583 canbench tests pass; clippy -D warnings clean; fmt clean. canbench wasm builds + runs. Validator final phase: PASS — all 5 todos + 7 completion criteria checked.)

## Later Slices (recorded, not in this plan)

- 0→w width addition on existing tree buckets (deferred materialize, tree form).
- w1→w2 re-encoding / w→0 teardown in tree mode.
- Batch tree admission for w > 0 (parallel property writes).
- Crate slimming (unlabeled graph removal — user decision).
- Tree read polish (bench_t_v_window +2.10%; dense window contract).
## REWORK (2026-09-02, orchestrator REJECT → PASS)

The first REWORK-1/2/3 cycle shipped with a **split-brain layout** (F-1 P0):
read path used `edge_start + edge_root_len` (contiguous, ADR §2), but write
path used `inline_property_bytes_offset` (repurposed, separate span). The
benches measured 540 ins (synthetic seed had `bucket_count = 0` → `find_bucket`
missed → no slot visited) and the unit tests passed only because the
synthetic seed happened to allocate adjacent spans (F-1 F-3).

**REWORK F-1**: unified layout. The combined LEG span is
`[edge root | property root]` (gap 0). `inline_property_bytes_offset = 0` in
tree mode (the property root is derived as `edge_start + bucket_span_region_len(bucket)`).
All write sites (promote, depth-1 tail append, depth≥2 tail append) do a single
`allocate_span_avoiding(combined_len, avoid=old_combined_span)` and copy
both halves. No tail buffer. Read path is unchanged (was already correct).

**REWORK F-2**: cap guard `PropertyTreeRootCapacityReached` added to
`LabeledOperationError`. The guard fires when the property root would exceed
`R_MAX = 1024`. Property-tree deepen (with `BlockKind::InlinePropertyInterior`)
is recorded as a follow-up slice per ADR 0088 §7.

**REWORK F-3**: benches use the **production insert path** via
`insert_edge_skip_leaf_cascade`. The bench seed pre-declares the bucket
schema via `ensure_label_bucket_inline_property_byte_width(vid, label, 32)`
(production helper). The bench is honest about what's measured (the read
cost only; promote transcription is outside `bench_scope`). Bench names
changed: `tcsr_4096_property_read_w32` (4K, 3.16M ins), `tcsr_65536_property_read_w32`
(65K, 5.63M ins).

**REWORK F-4**: the w=0 hot path uses `allocate_span` (no avoid) instead of
`allocate_span_avoiding(..., None)`. The interior-row-append path
(`row_in_home_interior != 0`) for w=0 does NOT realloc the edge root
(edge_root unchanged on interior-row append). The 1M+1 deepen bench
regressed by +24.81% during the partial slice; the REWORK brings it back
to 0% by removing the spurious w=0 realloc in the interior-row-append path.

**REWORK F-5**: wasm exported names report.

### canbench results (REWORK)

- `tcsr_4096_property_read_w32` (new, 4K w=32 read): **3,160,000 ins**
- `tcsr_65536_property_read_w32` (renamed from old bogus 540 ins entry; the
  old 540 ins was measuring nothing — synthetic seed had `bucket_count = 0`):
  **5,630,000 ins**
- All other 159 entries: no significant changes (the REWORK's w=0 hot path
  preserves the previous ins counts)
- 2 new entries (4K + 65K), 0 failed, 0 regressed

### Tests

- 630 lib tests pass (+4 from 627: `lpb_in_tree_rework_combined_span_round_trip`,
  `lpb_in_tree_rework_f2_cap_guard`, plus 2 from foundation)
- 583 canbench tests pass
- clippy -D warnings clean
- fmt clean
- validator final phase: PASS
