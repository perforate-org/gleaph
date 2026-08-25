# 0079. Two-tier precision encoding: RaBitQ code tier with same-page exact rerank

Date: 2026-08-24
Status: implemented
Last revised: 2026-08-24

> Numbering note: the Slice 6 working notes pre-reserved "ADR 0078" for this decision, but
> [0078] was taken in the interim by another pane (authorization-aware vector search). Per the
> shared-numbering protocol the next free number, 0079, is used instead.

## Context

The vector index advertised **stored precision = search precision** (design principles #4,
[ADR 0064] §3): every scanned row was scored by the full F32/I8 kernel over its original
bytes. At the design target (d = 1536) a full scan streams ~6 KB of original bytes per row
from stable memory and runs the F32 kernel over all of them, so first-stage scan cost scales
linearly with `dims × live_rows` even when only a handful of rows can make the final top-k.

Product-code retrieval tolerates a bounded, measurable approximation at the *first stage* as
long as the **advertised result quality stays the original tier**: the stored bytes remain the
single source of truth for scoring, ranking, idempotency comparison, and rebuild carry-forward.
[ADR 0064] §8's PQ option was rejected for v1 because product quantization replaces the stored
distance computation entirely (scores would no longer be exact anywhere), which the then-current
principle forbade. This ADR revises that principle instead of forcing an all-exact or all-approximate
choice.

## Decision

Adopt a **two-tier precision contract**:

1. **Original tier defines quality.** `VectorEncoding` stays `F32 | I8` (no new public
   encoding). Original bytes keep every existing role: exact rerank scores, same-stamp
   idempotency comparison (`prepare_for_metric` output only — code segments are never compared),
   rebuild pool carry-forward (the pool stores originals; codes are recomputed deterministically
   at shadow append), and wire query format (canonical F32, Model Y unchanged).

2. **Code tier accelerates the first stage only.** A generation built via
   `admin_start_vector_rebuild(code_tier = Some(true))` stores one extra per-row segment behind
   the original bytes on the same page:

   ```text
   code segment = [code_aux 8B: ‖x‖² f32 | φ_x f32][codes ceil(P/64)·8B],  P = next_pow2(dims)
   ```

   The codes are the sign bits of the seeded randomized Walsh–Hadamard rotation of the row
   (zero-pad to P → splitmix64-derived sign flips keyed by the def-frozen `rotation_seed` →
   unnormalized WHT → `1/√P` scaling), i.e. a 1-bit RaBitQ-style sketch. `PageHeader` gained a
   self-describing `code_stride` field (28 → 32 B); mixed generations coexist because reopen
   strictness applies only to active-generation pages and compaction spans derive from page
   headers. The def schema grew to 59 B (`GLEAPH-VECDEF-03`: `code_tier`, frozen
   `code_stride_bytes`, `rotation_seed`; fresh install required).

3. **Search: Stage A estimate → Stage B exact rerank.** When the scanned generation has codes,
   each loaded page is scored in two stages:

   - **Stage A** estimates every live row's squared distance with the two-sided binary
     estimator — XNOR+popcount against the per-query rotated binary sketch, normalized through
     the stored sketch correlations:
     `dist² ≈ ‖q‖² + ‖x‖² − 2‖q‖‖x‖·clamp(s/(φ_q·φ_x), −1, 1)` with
     `s = (2·pc − P)/P`. The top-`C` shortlist (`C = clamp(8k, 128..=1024)`, ties to the lowest
     subject) is kept per page.
   - **Stage B** rescors shortlist rows **exactly** from the already-loaded page scratch — zero
     additional stable reads, satisfying the no-separate-store boundary — into the global
     bounded top-k with the unchanged `(distance, subject)` tie-break.

   Rows may skip Stage B only under a provable lower bound: substituting both sketch
   decompositions `x̄ = φ_x·x̂ + r_x` into `s` bounds the residual by Cauchy–Schwarz
   (`|ε| ≤ φ_x√(1−φ_q²) + φ_q√(1−φ_x²) + √((1−φ_x²)(1−φ_q²))`), giving a distance lower bound
   computed from the stored aux alone. The bound never produces, reorders, or filters emitted
   hits; looseness costs speed only. Final output is the exact top-k over the union of
   shortlists — approximate over the index, exact within what it emits, recall measured in
   tests/benches (1.00 @k10 beyond the envelope on clustered fixtures).

4. **Why RaBitQ rather than PQ (v1).** Both sides binarize through one seeded orthogonal
   rotation, so (a) codes are 16–20× narrower than original bytes at d1536 (264 B vs 6144 B),
   (b) the estimator needs no training, no codebooks, and no distance tables — it is a bit-op
   plus a handful of flops fused beside the existing page walk, and (c) exactness is preserved
   at Stage B by construction because originals stay resident in the same scratch. PQ remains a
   candidate for a future A/B once a first-stage-compressed rerank-free variant is worth its
   training/ownership cost; adopting it would supersede this ADR's estimator but not the
   two-tier contract.

## Consequences

- Flat and two-level lifecycles accept the flag; the publish flip carries `code_tier` +
  `code_stride_bytes` into the definition, and search routes through Stage A/B exactly when the
  scanned generation stores codes. Filtered allowlist scans stay exact.
- Page capacity shrinks under the fixed byte budget when the tier is on (per-row code width);
  `slots_per_page` is rederived from `max_page_bytes` by one pure function (`shape_def_for`)
  shared by dual-write, Building batch, and publish paths.
- The rebuild-pool region header (format version 3) carries the shadow generation's `code_tier`
  flag from start to `Building`, keeping the Sampling/Training lifecycle records shape-minimal.
- Measured (wasm32 canbench, d1536, ε₂ = INF full walk, 4096 rows / 16 partitions): tier-on k10
  40.95M vs tier-off 43.48M instructions (−5.8%); stable byte streaming dominates e2e cost, so
  the arithmetic saving grows with future kernel work rather than layout change. Query rotation:
  777K ins per search. Tier-on upsert delta is inside noise on d128.

## References

- [ADR 0064] §3 principle revision (two-tier precision), §7 page geometry (`code_stride`),
  §8 encodings (RaBitQ vs PQ).
- Design: `design/index/vector-index.md` (Encodings / Search path / Growth model).
