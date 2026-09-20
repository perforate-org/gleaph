# 0096. Inline-tiny mode for low-degree label buckets (descriptor-resident, K=4)

Date: 2026-09-17
Status: accepted (D1 decision 2026-09-18; R3 implementation merge additionally requires passing G1–G6)
Last revised: 2026-09-18

## Context

Two measured inputs converge on a low-degree tier for labeled LARA:

- **S1** ([2026-09-17 investigation](../investigations/2026-09-17-lara-improvement-investigation.md#s1-result--core-insert-attribution-measured-2026-09-17-worktree-source)):
  PMA counts accounting is ~56% of the attributed core slab-append cost (~3.2K
  of ~5.7K ins/insert). Successor-boundary reads are ~12%. Slot write and vertex
  rewrite are near floor. A tier that skips per-insert counts/span bookkeeping
  has the largest single leverage on small-row inserts.
- **S3** (same note, §S3 result): out-degree census over the recorded Orkut
  inputs (shuffled directed prefixes, header excluded) gives K≤3 rows at
  61.7% of rows / 19.5% of edges (1M) and 37.1% / 5.0% (10M). Label splitting is
  a strict refinement of rows, so single-label coverage is a lower bound for
  multi-label tiny-bucket coverage.

The LARA-DRAM memo proposed a pooled-chunk tiny (K=8). On IC that design needs a
new stable region with inventory, composite reopen, and chunk free management
plus crash consistency — the memo's own caveat. The `LabelBucket` descriptor
(29 bytes, `labeled/record.rs:55`) already carries 13 dead bytes for a
property-less, log-less bucket, so a pooled store is unnecessary for labeled:
targets can live inside the descriptor. (Core's 16-byte vertex row has zero dead
space — every field load-bearing — so this design is labeled-only. Core tiny
would need row growth or a new region and is out of scope.)

Lineage: ADR 0022 cites Terrace (in-place low tier, PMA mid tier, tree/B-tree
high tier). This ADR is Gleaph's low tier; tree-CSR (ADR 0088) is the high tier;
the slab PMA stays the mid tier.

## Decision

Add a third bucket storage class, **tiny**: buckets with `degree ≤ 4` whose edge
targets live inline in the descriptor, hold zero slab slots, use no overflow
log, and carry no inline-property schema. The tier shipped at K=3 and was
extended to K=4 by §3b Phase 2 (not 8 — see §3).

### 1. Wire (29 bytes, unchanged size, no new region)

`BUCKET_TINY_MODE_BIT = 1 << 60` in the packed word (bits 61–62 stay reserved;
bit 63 stays the tree flag; tiny∧tree is invalid). `BUCKET_RESERVED_BITS_MASK`
shrinks to bits 61–62 with the same exclusion pattern as the tree bit.

| Bytes | Slab/tree meaning | Tiny meaning |
| --- | --- | --- |
| word: label [36..52] | label key | label key (unchanged — `find_bucket` needs no arm) |
| word: edge_start [0..36] | slab/root span start | **empty-span anchor** (valid successor boundary, §5) |
| word: log-head [52..60] | log head or NONE | MUST be `OVERFLOW_LOG_NONE` (a zero here would decode as a live log) |
| word: bits 60–63 | reserved/tree flag | bit 60 = 1, bits 61–62 = 0, bit 63 = 0 |
| [8..12] degree | live count | live count, 0..=4 (untouched — all degree readers keep working) |
| [12..16] stored_slots | slab width | **T3** (u32 LE inline target) |
| [16..20] / [20..24] / [24..28] | ipb fields | **T0 / T1 / T2** (u32 LE inline targets) |
| [28] | ipb log len | **used width** 0..=4 (live slots + tombstone holes; reserved zero under the retired K=3 wire) |

Tiny payload is the used prefix `[0..used)` of T0..T3: live slots hold targets,
dead slots hold the layout-native `E::tombstone_edge()` encoding (read via
`E::is_deleted_slot()`), and slots at or past `used` are never read (their
content is unconstrained — no zero-tail rule). `used` is stored explicitly
because neither of the two candidate derivations survives contact with the
contract: `VertexId(0)` is a legal live target, so a zeroed slot cannot mean
"dead"; and Insertion placement must append at the used tail without reusing an
interior hole (ADR 0052 §6, `insertion_placement_never_reuses_interior_tombstone`),
which requires knowing where the tail is. `degree` counts live slots inside the
prefix, so `degree ≤ used` and slot ordinals are descriptor-relative.

Consequences of this map: `degree` stays readable and meaningful (live count in
all modes), so degree readers need no arms. A tiny bucket has **no stored slot
width at all**: bytes 12..16 are payload, so the retired K=3 meaning is gone and
every geometry reader is served by the mode-aware `stored_slots()` accessor
(§3b Phase 1), while the tiny *bound* is `tiny_used_width()` (byte 28). Span and
occupancy paths treat tiny as zero-width, scan paths iterate `[0..used)` with the
layout liveness predicate, and placement reports zero reserved geometry for tiny
(see §5). The repurposed region (bytes 12..28) is only reachable through
inline-property paths, which are all width-gated (`values.rs` early-returns on
width 0; §4 makes width≠0 unrepresentable-persisted on tiny by enforcing it at
write boundaries, since the width bytes themselves are payload and cannot be
validated on the wire).

Validation matrix (extends the `try_read_from` / `try_from_parts` fail-closed
pattern; tree-mode exceptions are the precedent). Checkability differs by K
(see §3) — the table states the current K=4 rules:

| Rule | Slab | Tree | Tiny |
| --- | --- | --- | --- |
| reserved bits 61–62 zero | yes | yes | yes |
| tree bit 63 | 0 | 1 | 0 (tiny∧tree rejected) |
| tiny bit 60 | 0 | 0 | 1 |
| log-head byte | any valid | any valid | MUST be NONE |
| degree bound | u32 | u32 | ≤ 4 |
| used width | n/a | n/a | byte 28 ≤ 4 AND `degree` ≤ `used` |
| payload shape | n/a | n/a | slots `[0..used)` hold targets or layout-native tombstones; slots past `used` unconstrained |

Wire-checkability note: T2 (`bytes[24..28]`) spans three typed fields
(offset-hi, width, ipb log byte) and T3 is the whole former stored field, so on
a tiny bucket those four are payload and only enforceable at write boundaries —
never read except through the §5 entry arms (verified by G6). The used width
itself is a plain byte field, so it *is* wire-checkable; a drifted value can
only shrink (live slots unread) or expose unconstrained slots, which the liveness
predicate filters — the layout audit additionally requires live-slot count ==
`degree` inside the prefix.

Validation gradient (why K=2 is special): the retired K=3 wire carried one
jumbled target and the K=4 wire carries two (T2 and T3), so no K≥3 tiny bucket
is fully wire-checkable — its schema bytes ARE target payload and have no second
meaning to expose. Byte separation is impossible by construction; the design
separates PATHS instead (§5 entry order: tiny arm before every width check) and
proves it behaviorally (G6), with bounded degradation where proof lapses (valid
counts and anchors). The K=2 retreat is the fully-checkable wire (every
non-payload byte constant). If width-reader sprawl ever outgrows entry
discipline, the retreat is K=2, not more encapsulation.
| inline-property width | any | any (LPB) | inside T2 payload: NOT wire-checkable; enforced 0 at write boundaries (§4) + entry order (§5) |

### 2. Constants and mode representation

- `TINY_MAX_DEGREE: u32 = 4`; the tiny bound is `tiny_used_width()` (byte 28),
  not a stored width — bytes 12..16 are payload.
- `cap_for_mode(Tiny) = TINY_MAX_DEGREE` (a tiny bucket can never approach the
  slab/tree caps; the promote trigger fires first).
- `BucketMode` gains a `Tiny` variant; `from_bucket` maps tiny-bit → Tiny
  before the tree check. **Exhaustive-match rule:** new and touched dispatch
  points match on `BucketMode::from_bucket(..)` with all three arms and no
  wildcard, so the next mode forces a compiler decision at each point
  (implementation-integrity §2). Existing `is_tree_mode()` call sites outside
  the dispatch table are untouched.
- One semantic helper `is_tiny_mode()` next to `is_tree_mode()`; no scattered
  bit tests.
- Payload ownership: T0 ≡ `ipb_slab_slots` value, T1 ≡ `ipb_offset` low-32
  (+hi-zero validation rule), T2 ≡ raw composition, T3 ≡ the former
  `stored_slots` field — all four exposed ONLY through `tiny_target(i)` /
  `with_tiny_target(i, v)` methods, and the width through `tiny_used_width()` /
  `with_tiny_used_width()` (all debug-asserting tiny mode). Direct reads of the
  underlying fields as targets, and any second packing helper, are review
  rejections. `with_stored_slots` is slab/tree-only and debug-guards tiny input.

### 3. K ceiling (K=4 current; K=3 retired; K=2 fallback)

Byte-accounting the tail: T0≡`ipb_slab_slots` value, T1≡`ipb_offset` low-32
(+hi-zero rule) are clean single-field mappings. T2 (`bytes[24..28]`) crosses
three typed fields (offset-hi, width, ipb log byte) and needs a byte-composition
helper. That jumble is the real cliff — not the stored_slots count:

- **K=2: fully validatable, zero churn.** Payload is T0+T1 only; every other
  tail byte is a checkable constant (offset-hi 0, width 0, log byte NONE,
  byte 28 zero). No field repurposed, no reader touched. Coverage: 50.4% rows /
  13.0% edges (1M), 28.3% / 3.0% (10M). Specified as the **fallback**: K=2 wire
  reads cleanly under K=3 rules, so retreating is one constant plus validation
  tightening, with zero reader changes.
- **K=3 (retired):** the shipped wire. One jumbled target (T2) and a tiny
  stored width; superseded by K=4, which pays the encapsulation debt instead of
  the jumble debt.
- **K=4 (current, §3b Phase 2, `91575bc29`):** four targets. Costs the
  `stored_slots` repurposing plus byte 28; coverage over K=3 is +8pp rows /
  +6pp edges (1M) and +7pp / +2pp (10M). The encapsulation work Phase 1 landed
  first (`15719602c`), so no reader was audited by hand: the mode-aware accessor
  plus `tiny_used_width()` keep every geometry and count path decidable.
- K=8 inline is impossible (32 B > 29 B descriptor).
- A missed dispatch on tiny degrades to a bounded misread (valid counts and
  anchor; span reads confined to the tiny prefix), caught by the mode-matrix
  gate (G6).
### 3b. K=4 encapsulation design (landed)

The type is already ~90% encapsulated: the ONLY public fields are `degree`
and `stored_slots` (`record.rs:32-34`); `word`, all ipb fields, and the log
bytes sit behind method accessors (`edge_start()`, `overflow_log_head()`,
`inline_property_*()`), and external struct literals are already impossible
(private `word`). K=4 completes that encapsulation instead of auditing readers:

1. **Privatize `stored_slots`** (rename to `stored_slots_raw`) and add a
   mode-aware accessor returning the live count for tiny buckets (prefix scan
   with the layout liveness predicate — tombstone holes excluded) and the raw
   width otherwise. The compiler enumerates every
   direct reader (~300 sites) as an error; each is a mechanical swap to the
   accessor. No semantic decision per site.
   **Implemented 2026-09-20** (`15719602c`): the field is private, `stored_slots()`
   returns `degree` for tiny and the raw width otherwise, and
   `stored_slots_raw()` serves the sites that own the *wire* meaning —
   validation, tiny prefix iteration (ordinals include tombstone holes), and
   wire assertions: 240 accessor calls vs 74 raw calls, compiler-enumerated, with
   the full suite green (607 tests) as the zero-behavior-change proof. One test
   expression in `insert.rs` was re-pointed at the raw accessor (it asserts the
   tiny prefix width, not the live count); no expectation changed. `record.rs`
   keeps its internal reads on the raw field (the type owns the wire).
2. **Own the jumble**: `tiny_target(i)` / `with_tiny_target(i, v)` methods own
   the T0..T3 packing (T0≡stored_raw, T1≡ipb_slab, T2≡offset-lo32+hi0,
   T3≡bytes[24..28] raw composition) plus the zero-tail rule. Raw field access
   outside these methods + validation is a review rejection.
3. **Writers stay few**: `try_from_parts` validation per mode, promotion
   publish, tiny insert/delete — the only constructors of tiny state.
   `with_stored_slots` / `with_edge_range` remain slab/tree-only
   (tiny-unreachable by §5 dispatch, asserted).
4. **Two-phase slice** (code-quality reslicing): Phase 1 was the pure
   privatization with zero behavior change, proven by the full existing suite
   going green unmodified — it lands as standalone tech-debt paydown and needs
   no tiny decision. Phase 2 is tiny-K4 on top. **Timing: Phase 1 waited for
   the ADR 0088 Tree-CSR workstream to land** (same files, same readers —
   concurrent 300-site churn would have collided). That workstream is **landed
   in `main`** (2026-09-20 check: every tree plan lane/commit is an ancestor of
   `main` and no tree-semantic change is outstanding), so the timing condition
   is satisfied; the deciding question is now Phase 2's value, below.

**Phase 2 implemented 2026-09-20** (`91575bc29`). One deviation from the sketch
above: the retired plan reused the T2 jumble for T3 and left byte 28 reserved,
treating byte 28 as a mere convenience. That fails on two contract pinned by
tests: `VertexId(0)` is a legal live target (so "dead" cannot be a zeroed word),
and Insertion placement appends at the used tail without filling interior holes
(ADR 0052 §6) — both need an explicit used width. Byte 28 therefore became the
tiny used width (reserved zero under K≤3, so no transition shim is needed) and
T3 took the whole freed `stored_slots` field. The K=3 wire is retired: the crate
is pre-production, so there is one current tiny layout rather than a decoder
union.

Phase 2 also surfaced four defects the K=3 wire had been hiding, all fixed in
the same commit: the tiny matching scan was bounded by `degree` (hiding live
slots past the first hole) and offered dead slots to the predicate; promotion
had a single leaf-relocate retry, which a full 4-slot prefix exhausts;
`materialize_inline_property_stream_step` published schema fields onto a
still-tiny bucket (payload corruption, rejected on read); and
`compute_bucket_allocation` read T3 as a slot count, making `check_alloc_cap`
reject every tiny bucket. Each carries a regression or probe test
(`tiny_remove_matching_scans_holes_and_whole_used_width`,
`labeled_heavy_relocation_survives_reopen_without_corruption`,
`materialize_inline_property_stream_drains_via_bidi_maintenance_queue`,
`check_alloc_cap_tiny_mode_measures_the_used_width`,
`tiny_live_slot_count_invariant_catches_degree_drift`).

Measured (unfiltered `canbench --persist`, artifact `94fc62459`): the moved
boundary is worth 40-98 % on the workloads it covers —
`tiny_promote_d3_1024` 166.11 M → 3.92 M (-97.6 %, the degree-3 bucket no longer
promotes), `tiny_workload_4x1024` 401.89 M → 245.18 M (-39.0 %), G5
`tiny_workload_skewed_mix` 115.49 M → 113.15 M (-2.0 %); 191 benches, 50
improved, none above +0.25 % (code-layout variance).

K=4 delta gates (green in-suite): `tiny_wire_bytes_golden` (byte-exact row,
including T3 and the used-width byte), `tiny_roundtrip_degrees_0_to_4`,
`tiny_target_t2_splits_across_three_fields` (T2 composition unchanged),
`try_read_from_rejects_each_tiny_rule` / `try_enable_tiny_mode_rejects_each_rule`
(range and cross-field rules), `stored_slots_accessor_is_mode_aware`
(miss-degradation bound with a T3 target far outside any width), and the full
G1–G6 set re-run at K=4 as part of the suite.

Verdict: recommended sequence is K=3 now (bounded, self-contained), Phase 1 as
standalone tech-debt paydown once its value is decided, K=4 only if a production
census then still shows degree-4 buckets dominating the tiny-eligible set.
Degree-4 buckets work fine as slab buckets meanwhile — the boundary stays
optimization-only.

**Decision (2026-09-20, morning): Phase 1 and K=4 were deferred; not
scheduled.** The value arithmetic below stands as the context. **Owner direction
later the same day: pursue K=4** — Phase 1 landed (`15719602c`) and Phase 2
landed the same day (`91575bc29`, §3b). The S3 census re-check named as R5's
entry condition cannot be produced in-repo (no production telemetry), so it
remains a documented value check rather than a gate; the K=3 boundary stays
optimization-only either way.

Three measurements changed the arithmetic after K=3 shipped:

1. **The tier's main quantified leverage is now universal.** The S1 probe
   justified S4a with the per-insert PMA counts walk (56 % of the attributed
   labeled append cost). That walk was removed for *every* bucket on 2026-09-20
   (leaf-canonical counts, GAP-2026-09-20-004), so the tier's remaining benefit
   is the span/quota/log/window/cascade avoidance measured in the G1–G3 gates
   (~2× on a tiny insert) applied to the eligible subset only.
2. **Marginal coverage is small at scale.** K≤4 − K≤3 is +7.9pp rows / +5.9pp
   edges (1M) and **+7.0pp rows / +2.2pp edges (10M)** (S3 census). With the
   eligible *edge* share at 5.0 % (K≤3) → 7.2 % (K≤4), the marginal aggregate
   ingest effect is ≈1 %.
3. **Property-bearing buckets are excluded by construction** (width 0 required),
   and any single property-bearing edge raises its bucket's width, so the
   eligible set in property-bearing deployments is a subset of the census above
   (unmeasured; deployment-dependent).

Against that, Phase 1 costs a mechanical swap at every direct reader of the
crate's most-read field (~300 sites) and K=4 repurposes bytes 12..16, turning a
missed dispatch into an unbounded span misread. Revisit trigger: a *production*
census (degree **and** inline-property width) showing degree-4 buckets
dominating the tiny-eligible set — the same trigger the phase-2 wording already
names. Until then degree-4 buckets stay slab, which is correct, just
unoptimized.

### 4. State machine and transitions

Bucket storage classes: `{tiny, slab, tree}` plus the orthogonal vertex-level
bypass mode. R2b sequencing note: arms land first with birth deferred (slab
birth preserved, suite stays green — no producers exist); the birth flip is a
separate final commit with full-matrix green (carries a successor-anchor test;
birth is the only writer of initial anchors). Transitions once flipped: `tiny → slab → tree`. No direct `tiny → tree` (the dispatcher chains the two existing transitions in one insert, mirroring bypass→bucket→tree). No `slab → tiny` demotion in the first slice: tiny waste
is zero slab bytes, so a small slab bucket is merely unoptimized, never
incorrect — demotion needs its own benchmark gate (mirrors deferred tree
demotion). No oscillation is possible (promotion is one-way; deletes
tombstone inline and stay tiny — positional stability holds trivially since
survivors never move; slot-keyed counterpart occurrences observe stable
slots, see §5 delete row).

Birth rule (uniform, gated): every new bucket with 4-byte edges is born tiny
(spanless, pinless, quotaless — anchor from the successor boundary). Wider
edge types keep the slab birth path: tiny stores 4-byte targets and
transcription would be lossy (mirrors the tree carve-out; monomorphized away
per instantiation, so production `Edge` always takes the tiny path while 10 B
bench types keep existing behavior). Birth flips in a dedicated step AFTER all
arms plus the G6 matrix are green (R2b sequencing): flipping first would route
tiny buckets into unarmed paths. The flip commit carries a successor-anchor
test (birth is the only writer of initial anchors).

Promotion triggers (checked in this order at the insert dispatcher, before the
width check and the tree branch — width bytes are payload and must never be
read on a tiny bucket):

1. `bucket.is_tiny()`, prefix full with no hole (`used >= TINY_MAX_DEGREE`
   and every slot live), Insertion placement (the 4th edge; Unordered would
   hole-fill instead — ADR 0052 §6 parity holds across modes).
2. `bucket.is_tiny()` with insert width `w != 0`: promote first (transcribing
   live targets only — holes excluded; bucket width is conceptually 0), then
   proceed through the normal schema path. Tiny+width is never persisted.
3. `ensure_bucket_inline_property_schema_for_insert` (0→w on a non-empty tiny
   bucket): same promote-first rule, else typed reject (mirrors the tree
   carve-out shape).

`tiny → slab` promotion (reserve/commit/publish, modeled on
`promote_bypass_to_bucket_mode`):

1. Validate: tiny bit set, tree bit clear, `degree ≤ 4`, used width
   (`degree ≤ used ≤ 4`), bucket width 0.
2. Reserve (all fallible grows complete here): leaf pin + quota span through the
   existing `try_place_new_bucket_edge_span` / `ensure_…_span_room` path
   (a leaf relocate may fire here — that is the existing machinery, not new);
   counts/total capacity per the quota path.
3. Commit: `write_slots_contiguous` the live targets (tombstone holes
   excluded — the slab form is dense) into the new span (one call); publish
   one descriptor write clearing the tiny bit with `edge_start = span`,
   `used = degree`, ipb zeros; then fall through to the normal slab insert
   for the pending edge (reuses trigger, accounting, log admission — no
   duplicate logic).
4. Accounting: `+degree` leaf `actual` for the transcribed edges (never counted
   before) at promotion; the appended edge bumps normally. Total `+degree+1`.
5. Failure before publish releases the reserved span and returns typed error
   with canonical state untouched (rejection test seeds pre-existing state per
   implementation-integrity §3).

Cost bound: quota alloc + ≤3 slot writes + 2 descriptor writes + accounting.
Gate G3 caps it at 200 K instructions (bypass promotion measured 116 K).

### 4b. Capacity contract (tiny rows secure strictly less data capacity)

A tiny bucket consumes exactly one 29 B descriptor row and nothing else:

| Store | Tiny consumption | Enforced by |
| --- | --- | --- |
| Edge slab (`LEG`) | 0 slots — no span, quota, spill, or fold target | R2b: creation skips span placement via exhaustive match; slide resident-0; `debug_assert` in `try_place_new_bucket_edge_span` pins it now |
| Edge overflow log (per-leaf shared) | 0 entries — never admitted | R2b dispatcher order (tiny arm before any core call); G6 log-memory byte-counter proofs |
| Inline-property slab + log | 0 bytes/entries (width ≡ 0) | `debug_assert`s in `ensure_bucket_inline_property_bytes_span`, `materialize_inline_property_stream`, `fold_label_bucket_inline_property_bytes_log_to_slab` (R2b upgrades dispatcher-adjacent ones to typed guards mirroring the tree arms); allocated-funnel; G6 |
| Leaf block / PMA total | 0 (resident-0; excluded from `actual`) | slide/collect arms; density rule §5 |
| Vertex span (`LabeledVertex.stored_slots`) | 0 contribution | creation skips quota; promotion adds via the existing path |
| Bucket descriptor slab | 29 B/bucket — NOT reduced (metadata, not data) | by construction; descriptors move opaquely |
| Free spans | never allocates, never releases | follows from the above; no direct calls |

Sole-allocator rule: the ONLY span/log admission reachable with the tiny bit
set is the tiny→slab promotion reserve (§4), which runs pre-publish and
releases on failure. Every other allocation entry asserts `!is_tiny_mode()`
(debug today; typed guards at R2b where a dispatcher bug could route tiny
into them). Core `EdgeStore` slab/log admission has no bucket visibility, so
it cannot assert — the dispatcher order plus G6 zero-byte proofs own it there.

No-drain-staleness: queued materialize/fold work items never name tiny buckets
(enqueue sites divert at R2b). A bucket leaving tiny via promotion keeps queued
ops valid on the slab form (same edges; widths materialize post-promotion), so
no drain-side tiny logic is required.

Planning reading: leaf blocks and vertex spans exclude tiny rows entirely, so
the S3 row shares (44–70%) translate to span-management avoidance, not just
per-insert savings. Tiny is a data-capacity optimization; the 29 B descriptor
cost is unchanged and recorded as such.

G6 zero-allocation proofs (pending R2b): tiny insert/delete/scan with
`ReadMemory` byte-counters on edge-slab, edge-log, ipb-slab, and ipb-log
memories asserting zero bytes read AND written outside the 29 B descriptor row;
leaf `actual`/`total` unchanged; free-span store byte-identical.

### 5. Per-path rules (dispatch table)

| Path | Rule | Anchor |
| --- | --- | --- |
| Insert dispatcher | §7 match-first dispatch: Tiny arm first (structural — width/width-check/tree-branch only exist inside their arms). Unordered fills the first tombstone hole; Insertion dense-appends (`used` grows only there; `degree` always +1). Hole-less full prefixes and width-carrying inserts promote first (§4, transcribing live targets only, then recurse into dispatch). No counts bump, no successor read, no log. Location reports `Slab` storage class for not-overflow-log (tree-append precedent); ordinal is the slot. Placement parity with slab (ADR 0052 §6). | `insert.rs` (match-first dispatch) |
| Scan (`single_bucket_span_iter` — the funnel for all 6 visit/collect call sites) | tiny arm returns an inline iterator over `[0..used)`, skipping tombstone holes via the layout liveness predicate (`E::is_deleted_slot()` — same as slab/tree read paths), both orders, no stable reads. Placement: after the `degree == 0` early-out, before `successor_start` and `LabelEdgeSpanAccess` construction (which would present `[anchor, anchor+degree)` slab bytes). Tombstone-inclusive positions, so asc/desc are trivial reversals. | `traverse.rs` `single_bucket_span_iter` (match-first dispatch) |
| Property-visit entries (dense bulk-value paths, per-slot attach, bounded collect) | tiny diverts to the inline path (topology + empty values) at each entry: `visit_edges_with_inline_property_impl`, `visit_edges_for_label_impl`, `collect_edges_with_inline_property_bounded` (which otherwise fails valid tiny reads with spurious `LogChainShort`), `visit_edges_at_with_inline_property` (arm before slot selection — selection itself reads slab spans). All downstream `*_next` batch/log iterators are then unreachable for tiny. | `traverse.rs` (match-first dispatch in `visit_edges_with_inline_property_impl`; tree/slab share the slow path) |
| Value funnel (`is_inline_property_bytes_allocated`) | returns false for tiny (no value state by construction). This ONE arm covers every `!allocated \|\| width == 0` early-return path in `values.rs`, the readable predicate, and the resident computations — they all select the no-values path without further arms. Review check: any values path that proceeds on allocated-true must still be entry-diverted. | `record.rs` (allocated-funnel, unchanged) |
| Invariant predicates + audit (`invariants.rs`) | `bucket_dense_inline_property_batch_eligible`: explicit `!is_tiny` arm (it reads width directly; degree-3 payload with an 0xFF top byte would otherwise qualify). `bucket_dense_slab_inline_property_bytes_readable` and both resident helpers: covered by the allocated-funnel (short-circuit), no arms. Layout audit: tiny branch asserting the checkable wire rules (not a skip — unaudited state is debt). | `invariants.rs` (explicit tiny arms + tiny-branched audit) |
| Eligibility predicate (`out_bucket_inline_property_bytes_first_predicate_eligible`) | NO arm: the `overflow_log_head() >= 0` conjunct is false for tiny (NONE), so it correctly reports false. Recorded as verified-safe; G6 pins it. | `traverse.rs` (verified-safe predicate, no arm) |
| Telemetry (placement stats/info, value storage stats) | report 0 / skip tiny buckets (garbage width would otherwise corrupt stats or trap checked arithmetic). Route through the funneled resident helpers where possible. | `bucket.rs`/`values.rs` (tiny-skipped telemetry) |
| Fold planning (edge + value log folds) | value-fold loop: skip tiny explicitly (the ipb-log-head gate is payload for degree 3 and cannot be trusted). Edge-fold prepare: tiny contributes 0 to resident computation and counts as width-0 in the strategy predicate (it has no log and no values; reserving slab for it would be pure waste). | `compact.rs` (fold planning tiny-skips) |
| Batch planner | tiny guard at preflight head BEFORE the `:2077` width check (which would otherwise misfire `WidthMismatch` on payload bytes) and before any run math. `BucketFingerprint::from_bucket` snapshots are harmless pre-guard (garbage-but-stable compares equal) PROVIDED all consumption is post-guard — R2b audit item. | `batch_write.rs` (match-first dispatch in `preflight_run`) |
| Deferred enqueue + swap paths | `remove_side_compaction_should_fire`: false for non-slab (already specified). R2b audit items (verify, do not assume): `bucket_allows_unordered_swap` callers (none found — confirm dead/test-only), materialize/fold enqueue sites skip tiny, `ensure_*`/materialize have no direct external callers outside the dispatcher. | `deferred.rs`/`compact.rs`/`values.rs` (deferred/materialize entries, match-first) |
| Delete (`remove_edge_at_slot_with_move`) | §7 match-first dispatch: Tiny arm tombstones inline (no promotion) — writes the layout-native `E::tombstone_edge()` encoding, decrements `degree`, publishes; survivors never move so positional stability holds trivially (slot-keyed counterpart occurrences; moves-dropping `remove_edge_matching`/`remove_edge_at_slot` APIs report empty moves). Empty (`degree` 0) resets to clean-empty tiny (`used` 0; slots outside the prefix are unconstrained). No demote check (stays tiny; one-way promotion). | `remove.rs` (match-first dispatch) |
| Span geometry (`bucket_span_region_len`, `combined_span_region_len`) | tiny ⇒ 0, armed BEFORE the slab default (which would otherwise return the raw stored field — T3 payload on tiny — as a span length). Zero-width intervals make span-rewrite vertex logic skip tiny buckets with no further arms. | `compact.rs` (`bucket_span_region_len`/`combined_span_region_len` match-first) |
| Leaf slide collect (`rebalance_labeled_leaf_weighted_slide_in_block`) | resident width 0 for tiny (the enumeration reads `.stored_slots` directly and would otherwise assign degree slab slots to a spanless bucket, then materialize neighbor bytes as its edges). Arm at the `resident` fold. | `compact.rs` (slide resident fold, tiny-skipped) |
| Leaf slide materialize/commit (`materialize_labeled_vertex_edge_plan`, `commit_vertex_edge_span_layout`) | skip edge move for tiny (zero bytes); stamp `edge_start` with the running boundary so anchors stay valid across relocate/slide. Descriptors still move as opaque 29 B rows. | `compact.rs` (slide materialize/commit, tiny anchors) |
| Span publish log ownership (`materialize_labeled_vertex_edge_plan`, `commit_vertex_edge_span_layout`) | `fold_logs` names which owner keeps a bucket's edge overflow-log rows. `false` (rebalance) materializes the slab prefix alone and republishes the original `overflow_log_head`, because `compact_vertex_edge_span_one_step` folds one bucket's log itself (with `preferred_extra = log_len`) and asserts unchanged slot indices. `true` (rewrite, leaf relocation) materializes prefix + log, writes them as one run and clears the head, since a moved log's slots are stale. Both paths share this pair; only the policy argument differs. | `compact.rs` (publish pair, explicit `fold_logs`) |
| Successor chain (new-bucket placement) | `bucket_successor_start_after_bucket_for_new_bucket`: when the PRECEDING bucket is tiny, return its anchor (not `anchor + degree` — tiny spans are empty, so the stored-count formula would overlap the next span). Placement before a tiny next needs no arm (anchor reads correctly). | `bucket.rs` (tiny-aware successor chain) |
| Successor chain | tiny publishes `edge_start` = valid empty-span anchor (successor boundary at creation, like zero-length slab buckets). Contiguity helpers (`label_buckets_allow_contiguous_slab_copy` et al.) keep working: `degree ≤ used ≤ 4`, log NONE, anchors contiguous. **Requirement on the implementation slice:** span rewrite/slide must stamp spanless buckets with the running boundary (they already enumerate bucket rows opaquely — descriptors move as 29 B rows via `write_label_bucket_slots_contiguous`); G4 proves anchors + payload survive relocate byte-identical. | `bucket.rs` (contiguity helpers, anchor-preserving) |
| Leaf density | `actual` counts **live edge records that occupy edge-slab (PMA) slots** — slab/log rows. Tiny rows live inline in the descriptor and tree rows live in LTB blocks, so neither counts: counting them would inflate density into spurious relocates and post-promotion cascades. Tiny inserts/deletes skip the ±1 `actual` bumps (`insert.rs`/`remove.rs` (density exclusion)); tree inserts/removes skip them structurally (the tree helpers no longer take a vertex id, so they cannot address per-vertex accounting). Both non-slab modes adjust only at their mode transition: tiny→slab `+degree` (`promote_tiny_to_slab`), slab→tree `−degree` (`promote_bypass_to_tree_mode`; LARA `segment_actual[leaf] -= live`), tree→slab `+degree` (`tree_mode_demote_to_slab`). `total`-only corrections (`compact.rs`/`bypass.rs` (total-only corrections, untouched)) are untouched. Landed 2026-09-20: `tree_mode_leaf_actual_counts_slab_edges_only` (production insert/remove/promote/demote paths + leaf audit at every step) and the tree-run assertion in `batch_run_admits_tree_mode_bucket_tail_fit`; this resolves the density-accounting half of GAP-2026-09-17-001. | accounting inventory |
| Vertex span | tiny contributes 0 to `LabeledVertex.stored_slots`; promotion adds its span through the existing quota path. | `insert.rs` `promote_tiny_to_slab` |
| Batch planner | `is_tiny → Unsupported` at the head of `preflight_run` (same placement as the tree guard) → existing scalar fallback, which promotes naturally. No tree-style widening in the first slice. | `batch_write.rs` (match-first dispatch in `preflight_run`) |
| Deferred maintenance | tiny never enqueues work items; `remove_side_compaction_should_fire` returns false for non-slab modes (generalize the existing tree early-out, behavior-preserving for tree). | `deferred.rs` (remove-side trigger, non-slab false) |
| CounterpartScan | rank/select logical widths treat zero-reserved tiny placement as degree-wide; tombstone holes are skipped via the layout predicate in both rank (`count_preceding_*`) and select paths, so occurrence ranks stay stable. Slot↔ordinal identity is tombstone-inclusive (tree parity). | `traverse.rs` (rank/select inline arms) |
| Bypass | orthogonal (vertex-level vs bucket-level); no interaction. | — |
| New-bucket creation | tiny creation skips span placement (`ensure_labeled_bucket_edge_span_room` — no quota, no pin) and publishes anchor + NONE log-head + zero tail. `find_bucket` needs no arm (label-key comparison only). Birth is E::BYTES-gated (§4) and flips after matrix-green (R2b sequencing, with successor-anchor test). | `insert.rs` (E-gated tiny birth) |
| Bucket lifecycle | tiny follows the existing bucket lifecycle (empty buckets persist/prune exactly like empty slab ones — audit item for the slice, not a new rule). | — |
| Rank/select helper (`visit_live_edge_slots_until`) | dedicated find + inline loop over `[0..used)`, holes skipped via the layout predicate (counterpart primitives observe identical logical slots). No signature change; +1 bounded descriptor read, documented. | `traverse.rs` (rank/select inline) |
| Dense fast path (`visit_edges`) | slab-only inside the Slab arm (arm-guaranteed; no mode guards — the 2% bench parity path). Tiny has its own arm (inline); tree walks the LTB. | `traverse.rs` (Tiny arm of `visit_edges` match-first) |
| Windowed visit (`visit_edges_window`) | self-contained inline window loop over tombstone-inclusive `[0..used)` positions (holes consume position space but yield nothing — ADR 0088 §2 parity with slab/tree windows). | `traverse.rs` (Tiny arm of `visit_edges_window` match-first) |
| Topology collect | inline loop over `[0..used)`, holes skipped via the layout predicate; live-count check passes. (The bounded-collect API variant was removed before landing — cap contract lives in unit tests.) | `traverse.rs` (collect inline) |
| Batch-value entries (`visit_out_*_batches_for_label_next` ×2) | early-return `Continue` (emit nothing — bypass precedent for valueless rows). Downstream dense/sparse/log/batch helpers unreachable. | `traverse.rs` (batch-value entries, tiny early-return) |
| Per-slot select (`visit_edges_at_with_inline_property`) | force-canonical (`is_tiny` OR-ed into the candidate flag): the canonical visitor's funnel arm serves inline ordinals with selected-filtering. Direct slot reads never run for tiny. | `traverse.rs` (per-slot select, force-canonical) |
| Matching remove (`remove_edge_matching_skip_leaf_cascade_with_move`) | inline linear match over live slots (holes skipped via the layout predicate; label-attached edges, mirroring slab predicate input) delegating to the positional tiny remover. | `remove.rs` (match-first dispatch) |
| Inline iterator (`LabeledSpanIter::Inline`) | pre-materialized `(ordinal, edge)` pairs, direction flag, `next`/`next_with_slot`/`try_advance_by` arms (shortfall contract mirrored). No stable reads. | `iter.rs` (`LabeledSpanIter::Inline`) |
| Rewrite paths (bulk gate, else-collect, else-row, sizing) | bulk gate excludes tiny (`any tiny → false`); else-collect pushes empty runs (index alignment); else-row stamps anchors preserving used/degree/holes; sizing loops (`read_and_plan`, relocate planning, stepped trigger) skip tiny (0 resident). Metadata-only republish needs no arm (anchor+degree shape already correct). | `compact.rs`/`bucket.rs` (rewrite/sizing tiny-skips) |
| Position calculators (`calculate_label_edge_span_positions*`) | resident/advance 0 for tiny (positions pack them at the running boundary); gap weights intentionally unchanged (free-space distribution only). | `compact.rs` (position calculators, tiny resident/advance 0) |
| First-compaction move | early `Ok(None)` (tiny needs no moves — holes are inline state, not slab slack); swap/move finders inherit coverage (sole caller). | `compact.rs` (first-compaction move, tiny-early None) |
| Placement projection (`read_label_bucket_placement_info`) | tiny reports honest degree with zeroed geometry (never sizes planners from payload). Currently caller-less outside tests; armed for future planners. | `bucket.rs` (placement projection, zeroed geometry) |
| Rank/select helper (`visit_live_edge_slots_until`) | dedicated find + inline loop over `[0..used)`, holes skipped via the layout predicate (counterpart primitives observe identical logical slots). No signature change; +1 bounded descriptor read, documented. | `traverse.rs` (rank/select inline) |
| Dense fast path (`visit_edges`) | slab-only inside the Slab arm (arm-guaranteed; no mode guards — the 2% bench parity path). Tiny has its own arm (inline); tree walks the LTB. | `traverse.rs` (Tiny arm of `visit_edges` match-first) |
| Topology collect | inline loop over `[0..used)`, holes skipped via the layout predicate; live-count check passes. (The bounded-collect API variant was removed before landing — cap contract lives in unit tests.) | `traverse.rs` (collect inline) |
| Batch-value entries (`visit_out_*_batches_for_label_next` ×2) | early-return `Continue` (emit nothing — bypass precedent for valueless rows). Downstream dense/sparse/log/batch helpers unreachable. | `traverse.rs` (batch-value entries, tiny early-return) |
| Per-slot select (`visit_edges_at_with_inline_property`) | force-canonical (`is_tiny` OR-ed into the candidate flag): the canonical visitor's funnel arm serves inline ordinals with selected-filtering. Direct slot reads never run for tiny. | `traverse.rs` (per-slot select, force-canonical) |
| Matching remove (`remove_edge_matching_skip_leaf_cascade_with_move`) | inline linear match (label-attached edges, mirroring slab predicate input) delegating to the positional tiny remover. | `remove.rs` (match-first dispatch) |
| Inline iterator (`LabeledSpanIter::Inline`) | pre-materialized `(ordinal, edge)` pairs, direction flag, `next`/`next_with_slot`/`try_advance_by` arms (shortfall contract mirrored). No stable reads. | `iter.rs` (`LabeledSpanIter::Inline`) |
| Rewrite paths (bulk gate, else-collect, else-row, sizing) | bulk gate excludes tiny (`any tiny → false`); else-collect pushes empty runs (index alignment); else-row stamps anchors preserving used/degree/holes; sizing loops (`read_and_plan`, relocate planning) skip tiny (0 resident). Metadata-only republish needs no arm (anchor+degree shape already correct). | `compact.rs`/`bucket.rs` (rewrite/sizing tiny-skips) |
| Rewrite publish (landed 2026-09-20) | the four inline branches (disjoint copy, bulk slab copy, collected runs, else-row) are gone: every rewrite materializes through `materialize_labeled_vertex_edge_plan(leaf, &buckets, fold_logs = true, compact)` and publishes through `commit_vertex_edge_span_layout`, so descriptors, content runs, the vertex row and placement preference have one owner. Tiny is zero resident in both the planner budget (`bucket_resident_rows` skips it) and the commit-side position calculator, and the planner no longer computes positions. | `compact.rs` (single publish path) |
| Cursor/compaction internals (1744/1799/1915-style slot loops) | covered by trigger + fold arms (never invoked for tiny); G6 pins behavior. No direct arms (noise over safety). | — |

Scan-contract note (`lara.md` §1): tiny scans read one descriptor and no PMA
state — strictly narrower than the contract allows, hence compliant.

### 6. Measurement gates (prototype AFTER acceptance)

Per ADR 0022/0088 discipline — implementation does not begin until these pass
on a bench-gated prototype; any fail amends or rejects this decision:

1. **G1 tiny insert** (degrees 0→1, 1→2, 2→3) vs labeled slab insert at the same
   degrees. Target: ≤ slab (hypothesis from S1: ~1–2 K vs ~5–8 K).
2. **G2 tiny scan** per edge vs slab scan. Target: ≤ slab (one 29 B read vs
   descriptor + span reads).
3. **G3 tiny→slab promotion** (3+1 edges w=0; plus a w-carrying variant).
   Target: ≤ 200 K ins total.
4. **G4 relocate with tiny neighbors**: completes; tiny payload + anchors
   byte-identical post-relocate; cost ≤ baseline relocate (zero edge-byte
   copies for tiny rows — structural).
5. **G5 skewed synthetic workload** (labeled, Orkut-like degree mix at small
   scale): total ins + stable pages vs all-slab. Target: improve or neutral
   with a capacity win.
6. **G6 mode matrix**: every bucket-mode × {insert, delete, scan asc/desc,
   property-visit entries, predicates, fold planning with tiny neighbors,
   telemetry with tiny buckets, batch classify, width-mismatch op} behaves per
   §5, reusing the `ReadMemory` byte-counter pattern to assert zero slab/log/span
   reads on tiny paths. A wrong implementation that skips any tiny arm must fail.
   Fold-planning and telemetry entries are first-class matrix rows, not footnotes.
   Hole rows (delete redesign): tiny-holed buckets × {scan asc/desc skips holes,
   positional stability, hole-fill (Unordered) vs append-then-promote (Insertion),
   promotion transcribes live-only, compact preserves wire state} — pinned by
   `tiny_hole_uses_layout_native_tombstone_predicate` (high-bit layout) plus the
   slab-reseed fixture migrations.

### 7. Code readability & maintenance requirements (binding on the slice)

Intent: a future reader must reconstruct every §1/§4/§5 decision from code +
tests alone, and drift must fail review or tests — not rely on memory. Each
mechanism below is review-checkable:

1. **Match-first dispatch (make illegal order unrepresentable).** Restructure
each §5 entry point as `match BucketMode::from_bucket(&bucket)` FIRST, with
arm-local handling — notably the width check moves INSIDE the Slab arm (today
it precedes the tree branch at `insert.rs` (pre-§7 width-check position)). "Tiny arm before width
check" then stops being an ordering convention and becomes structural: width
is only readable inside arms whose mode gives it meaning. The Tiny arm handles
width-carrying inserts by promote-first plus recursion into dispatch, mirroring
bypass→bucket→tree chaining. Review check: any `inline_property_byte_width()`
or ipb-field read outside a Slab/Tree arm is a rejection — EXCEPT through the
allocated-funnel and the verified-safe predicate (see §5 table), which are the
two named exceptions. Applies to: insert dispatcher, `single_bucket_span_iter`,
property-visit entries, `remove_edge_at_slot_with_move`, batch
preflight, materialize/ensure entries, fold planning loops. **Loop-filter
exception:** per-bucket `if is_tiny { continue/skip/0 }` filters inside
multi-bucket fold/slide/sizing loops (`compact.rs` resident folds, slice
collects, materialize/commit rows, rewrite sizing, retire-interval filters,
first-non-tiny anchor finds) stay as filters, not matches — they enumerate
rows whose per-row handling is identical textually and whose mode decision
is the skip itself. A match per row would be noise over safety there; §7
review checks the filter predicate (`!is_tiny` / `is_tiny → 0/skip`) instead.
2. **Per-mode field docs at declaration.** Every repurposed or constrained
field/method documents all three modes' semantics inline (record.rs
tree-precedent: `tree_mode_physical_depth` docs): `ipb_slab_slots` (T0),
`ipb_offset` (T1 low-32 + hi-zero), width/log-byte/byte-28 (T2 composition),
`stored_slots` (== degree invariant for tiny), `edge_start` (anchor), log-head
(NONE), `degree` (0..=4), `used` (byte 28, 0..=4). A field whose tiny meaning is missing from its doc
comment is a review rejection.
3. **Cite-section comments.** Every tiny arm carries
`// ADR 0096 §N: <one-line mode-semantic reason>` (repo Plan/GAP citation
precedent). Comments restating WHAT the code does are rejected; they must state
WHY the mode semantics require it.
4. **Exhaustive matches, no wildcards** (§2 rule, enforced here as review
gate), plus a mode registry test that fails when a variant is added without
G6 matrix coverage (`bucket_mode_maps_tiny_and_caps_at_max_degree` in
graph.rs asserts all three variants classify with their caps and zero-cap
tiny geometry — extend it, plus the §4b/§6 matrix, for any new mode).
5. **Assertion-backed invariants.** Each §4/§5 invariant names its test:
density exclusion → counts-unchanged test (`tiny_insert_skips_leaf_actual_counts`,
`tree_mode_leaf_actual_counts_slab_edges_only`); zero LEG bytes → ReadMemory
zero-read test; anchor validity → relocate-preservation + successor tests;
promotion accounting → exact actual/total test; failure atomicity →
pre-existing-state-survives rejection test. A comment claiming an invariant
without a linked assertion is P2.
6. **Test names encode mode×op×expectation** (repo precedent), e.g.
`tiny_insert_skips_leaf_actual_counts`,
`successor_start_after_tiny_returns_anchor`, `tiny_scan_reads_zero_slab_bytes`.
7. **No second packing helper** (§2 rule restated): all T0..T3 and used-width access through
`tiny_target` / `with_tiny_target` + validation.

## Consequences

- One `BucketMode` variant and ~17 dispatch arms (§5 table: insert/scan/delete/
  property entries/funnel/invariants/telemetry/fold/batch/deferred/slide); validation matrix grows by
  one column. No wire-size, MemoryId, or reopen-topology change.
- Every future bucket-mode decision pays a third arm — mitigated by the
  exhaustive-match rule (§2), which turns silent misses into compile errors.
- K=4 is fixed by the wire: raising it is a new layout ADR, not a constant bump.
  K=2 retreat costs one constant plus validation tightening; it also needs the
  `stored_slots`/byte-28 payload retired, since the K=2 wire leaves both at
  their constants.
- Restructure tripwire (2026-09-17): no breaking redesign is warranted now —
  dispatch is O(1) bit tests, tiny/tree REDUCE relocate coefficients (fewer
  bytes move), no tier is removable without regressing its regime, and the 29 B
  wire holds K=4 with bits 61–62 to spare. Revisit structure only when a
  fourth descriptor-byte demand arrives (overlaying a fourth reinterpretation
  instead of widening/splitting would cross from essential Terrace-tier
  complexity into accidental overloading).
- Slab buckets with degree ≤ 4 keep working unchanged; the boundary is
  optimization-only.
- Blocks on nothing except acceptance; but see GAP-2026-09-17-001 — the
  promotion and relocate paths this design rides on are themselves under a
  correctness gap. The G4/G5 gates will exercise exactly that interplay; if the
  gap is still open at gate time, gates block (correctly).

## Alternatives considered

- **Pooled-chunk tiny (memo K=8):** rejected for labeled — region inventory,
  composite reopen, and chunk free management with crash consistency, against an
  inline design that fits K≤3 with none of it. The memo's own caveat agrees.
  (Core tiny still needs this shape and stays out of scope.)
- **K=4 (adopted):** via §3b encapsulation (privatize `stored_slots` + owned
  byte-composer + byte-28 used width). The reader audit became compiler-checked
  mechanical churn, and Phase 2 landed on top of it (`91575bc29`); the census
  trigger stayed a documented value check because no in-repo production
  telemetry exists.
- **K=8 inline:** rejected — 32 B of targets cannot fit a 29 B descriptor.
- **K=2:** specified fallback (fully validatable, zero churn, 50.4%/13.0% at
  1M) — retreat path if the T2 jumble proves heavier than estimated.
- **Tiny in the 16 B vertex row (core/bypass):** rejected — zero dead space;
  different owning concept (row degree vs label homogeneity for bypass).
  different owning concept (row degree vs label homogeneity for bypass).
- **Do nothing:** rejected — S1 leverage plus S3 coverage clear the bar this
  repo sets for storage tiers (cf. ADR 0088 gates).

## Documents to update on acceptance (not in this patch)

- ADR 0088 §7 mode machine (adds the pre-slab inline state; tiny→slab→tree),
  §3 bounds, constants registry (`TINY_MAX_DEGREE` policy).
- `design/storage/lara.md` labeled-alignment table; `lara-dgap-contract.md`
  bucket-row content.
- Design accepted at D1 (2026-09-18) on the recon pack plus the
  APPROVE-WITH-CHANGES verdict; the R2b audit items are merge conditions, not
  acceptance blockers. R3 implementation merge additionally requires passing
  G1–G6 (unchanged).
- Sequencing through K=4 (stages, gates, owners, decision points):
  [Plan 0361](../../plans/0361-inline-tiny-k4-roadmap.md).
