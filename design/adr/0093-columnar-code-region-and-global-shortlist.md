# 0093. Columnar code region + global shortlist for the tiered vector page store

Date: 2026-08-26
Status: draft
Last revised: 2026-08-26

## Context

ADR 0079's tier-on measurement (−5.8% vs tier-off, d1536/ε₂=INF/4096 rows/16 partitions) has
inverted in the current tree: **tier-on is now a net loss** (k10 47.03M vs tier-off 45.80M
instructions). Slice 7/8 (page-directory arithmetic, free list, L2 block bound) reduced the
tier-off byte streaming, while the tier-on path streams its inline per-row code bytes
(264 B/row) as pure additional volume — codes live inside the row slot, so every Stage A read
streams the original vector bytes too.

The A1 measurement (2026-08-26, wasm32 canbench, same fixture, top-k verified identical
k10 10/10 / k100 100/100):

| variant | k10 (C=128) | k100 (C=800) |
| --- | --- | --- |
| ① inline + per-page shortlist (current) | 47.03M | 51.46M |
| ② inline + global shortlist (follow-up) | 50.42M (+7.2%) | 59.51M (+15.6%) |
| ③ **columnar code region + global shortlist** | **23.80M (−49.4%)** | **32.70M (−36.5%)** |

Decomposition: global shortlist alone is a **regression** (Stage B re-reads scattered rows +
second-pass decode outweigh the arithmetic saving; the zero-extra-read variant needs a
C×6144 B heap buffer ≈ 25 MB memcpy). **The separation's pure effect is k10 −52.8% / k100
−45.0%** — the win is ~100% byte streaming reduction (Stage A streams only the code region,
1.18 MB; Stage B touches only the shortlist's original pages, 11 pages at k10), with arithmetic
reduction secondary. At d1536 F32 the inline layout streams 27.9 MB where the separated layout
streams ≈ 1.9 MB total.

## Decision

1. **The vector page store gains a dedicated columnar code region** per partition (second slab
   region or a separate page type — implementation's choice under the constraints below): rows
   keep `[header | run table | meta | vector]` and regain **full slots_per_page capacity**
   (no per-row code width shrink), while the 1-bit codes live columnar in the code region
   (244 rows/page measured, meta attached).
2. **Global shortlist (ADR 0079 §3 boundary revision).** Stage A streams only
   `[header | run | meta | code]` and competes all scanned rows for a **global top-C**
   shortlist (C = clamp(8k, 128..1024)) — not per-page. Stage B loads **only the original
   pages holding shortlist rows** and exact-rescores them. Stage B exactness is unchanged
   (originals in the loaded pages, same scratch semantics); the recall contract is re-pinned:
   the global top-C over the full candidate scan dominates per-page top-C recall, and C ≥ k
   preserves the deterministic top-k contract.
3. **Truncation/cap semantics unchanged**: no result cap (ADR 0034 Slice 5 multiplicity);
   exactness preserved by construction (ADR 0079 Stage B).
4. **Carry-through surfaces**: write path dual-writes codes into the region at upsert/batch;
   reopen validates the code region fail-closed; compaction rewrites both regions;
   the rebuild pool carries the shadow generation's code column through to `Building`; the
   page-store layout version bumps (reinstall per the fresh-install policy; no migration
   reader). `VectorIndexDef` is unchanged (`code_stride_bytes` already frozen; the code region
   geometry derives from `code_stride_bytes` + row count per partition like the row pages).
5. **Scope guard**: the receipt (ADR 0092), the filter/allowlist exact paths, and the two-level
   lifecycle are untouched; the change is confined to the tier-on scan path, the page-store
   layout, and the write/reopen/compact/pool surfaces.

## Consequences

- Measured targets (must hold at landing, ±5%): k10 ≈ 23.8M instructions, k100 ≈ 32.7M; Stage A
  streaming ≈ 1.18 MB; Stage B ≈ 0.68 MB / 11 pages (k10). Top-k identical to inline tier-on.
- Tier-on returns to a large net win over tier-off (−48.0% at k10), restoring and exceeding the
  ADR 0079 intent; ADR 0079 gains an erratum noting the inline-layout inversion and this ADR as
  the fix.
- Implementation cost (medium): page-store second region + write path + reopen strictness +
  compaction + rebuild-pool carry-forward; the SEARCH semantics, deepening (ADR 0092), and the
  Candid surfaces are untouched.
- B2 (Binary/SIGN) narrows the code width further and widens the separation win — this ADR is
  its natural predecessor.
- Non-goals: per-page shortlist retention, heap-buffered rerank candidates (measured worse:
  ≈25 MB memcpy), persistence of receipts, multi-SEARCH receipts.

## Verification plan

- canbench: the A1 fixture A/B at landing (targets above); eps sweep and rebuild benches stay
  within noise; top-k identity vs inline tier-on (k10/k100).
- reopen/compact roundtrip with code regions; rebuild carry-forward of the code column;
  fail-closed on version/geometry mismatch.
- E2E: adr0034 families, adr0082/adr0078/adr0031 stay green (the change is layout-level).