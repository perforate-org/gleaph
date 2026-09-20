# LARA Orkut feedback triage (2026-09-17)

Date: 2026-09-17
Status: Triaged — recorded as external measurement-only evidence; no contract change, no implementation
Source: `/Users/yota/dev/lara/docs/orkut-compare-2026-09-17/v1/feedback-to-gleaph.md`
Source baseline: `95d791087`

## Scope limits (restated from the memo, not re-measured here)

- DRAM, single-thread, host counters only. No PMEM, generality, No-EL-ratio, or full-Orkut LARA claims.
- Scan checksum parity confirmed on subset/mid only. Full-Orkut LARA values were not measured
  (host constraint: extrapolated 17 GB vs 3.3 GB reclaimable) and nothing below extrapolates to them.
- All numbers below are quoted from the memo for disposition purposes; Gleaph adopts none of them
  as its own measurements.

## Item 1 — Tree promotion threshold (T=1024 effective in LARA DRAM)

Memo evidence: at Orkut-mid 10M with `T_PROMOTE=1024` / `T_DEMOTE=512`, 108 tree rows hold
205,555 edges (2.1%), including the max hub (degree 2,023); early isolation of high-degree rows
bounds slab-rebalance scope and contributes to the measured write reduction (0.037 vs C++).

Gleaph state:

- Labeled: `T_PROMOTE=4096` / `T_DEMOTE=2048` at triage time
  (`crates/ic-stable-lara/src/labeled/graph.rs`), tree-CSR mode per
  [ADR 0088](../adr/0088-tree-csr-mode-for-high-degree-label-buckets.md)
  (implementation in progress). `T_promote` is a benchmark-gated policy constant, set larger
  for IC stable-memory write economics. **Adopted `T_PROMOTE=1024` / `T_DEMOTE=512` on
  2026-09-19** once Gleaph-side canbench evidence existed (see the disposition below).
- Core (unlabeled) LARA: all-slab, no tree mode. Tree as a second instance is deferred
  (ADR 0088 Plan 0321), not rejected.

Disposition: supporting evidence for "early hub isolation bounds rebalance scope" —
reconfirmed by Gleaph-side canbench evidence, and the threshold was adopted on
2026-09-19 as `T_PROMOTE = 1024` / `T_DEMOTE = 512`
(`crates/ic-stable-lara/src/labeled/graph.rs`). The DRAM memo numbers were not used
as evidence for the IC decision: after the leaf-density accounting fix
(GAP-2026-09-17-001) and the overflow-log promotion fix (GAP-2026-09-19-001), the
equal-work and workload-level IC measurements favor 1024 — M1 hub growth 8192
41.70M vs 55.87M (equal work), M2a scan 40.09K vs 74.30K, steady-state append
4.46K vs 8.36K, G5 skewed mix 132.24M vs 140.11M — with only single deletes
(7.33K vs 4.84K) and one-off block-boundary mints favoring 4096. Full table and
verdict: [2026-09-17 improvement investigation](2026-09-17-lara-improvement-investigation.md)
and [implementation-gaps §GAP-2026-09-17-001](../implementation-gaps.md). The
core-tree question stays with Plan 0321; ADR 0088 "Threshold re-tune" records the
constant.

## Item 2 — Low-degree non-slab storage (tiny K=8 pooled chunk)

Memo evidence: at Orkut 10M, 420,136 tiny rows hold 1,361,358 edges (13.6%) outside slab
allocation/rebalance; degree 1–8 rows scan in one `degree×4`-byte read with no slab span
management (43.6% at the 1M subset — the effect grows as low-degree rows dominate).

Gleaph state: no degree-based tiny tier exists in core or labeled LARA. The labeled bypass mode
is a default-label homogeneity fast path, not a degree tier — different owning concept
(label homogeneity vs row degree), so it cannot absorb this change without weakening
encapsulation.

Disposition: **deferred concept, no code change**. An IC-stable port needs its own
allocation/free/persistence/crash-consistency design (stable-region inventory per
[ADR 0007](../adr/0007-stable-memory-layout.md)); DRAM slab-savings numbers are not portable.
If ever pursued, it requires its own ADR as a new storage concept — not an extension of bypass.

## Item 3 — No-edge-log negative results (keep the per-segment log)

Memo evidence (both No-EL redesign attempts failed, preserved as negative results):

- Retry-until-fit reaches a fixed point: at the 13,883rd insert, density 0.59, 1,024 rebalances
  still leave no free slot in the target row — proportional-gap placement guarantees no
  per-vertex minimum free slot.
- Window-expansion plus forced gap breaks the segment-meta invariant (`total_space == capacity`)
  via single-row index edits — the reason the base implementation has no such primitive.
- The paper's No-EL is a redesigned insert path, not a flag flip. EL write cost at Orkut-mid is
  12.44 GB (106 B/edge, 26.5× payload); the No-EL ratio is unestablished.

Gleaph state: per-segment overflow log with minimal entries (`prev` + payload only,
`crates/ic-stable-lara/src/lara/edge/log.rs`) plus bucket/vertex records — the failure modes
above map onto invariants Gleaph already enforces ([lara.md](../storage/lara.md) §2
vertex-local update with per-leaf log overflow; §4 free-span only after relocate).

Disposition: supporting evidence for **keeping** the per-segment overflow-log design.
No design change, no removal attempt.

## Excluded items (no action)

- **Log fine-grained reads**: Gleaph log entries are already minimal (`prev` + payload); parity
  was achieved from the other direction.
- **Slab batched reads**: Gleaph's read path is already more thorough (`read_slots_contiguous`
  + dense bucket batching + chunk prefetch). Mutual confirmation at most.
- **Probe bitmap**: host-counter optimization, irrelevant to canbench (IC instruction counting).

## Verdict

- Checked: `design/storage/lara.md`, `design/storage/lara-dgap-contract.md`, ADR 0088,
  ADR 0022 Stage-2 status. All remain valid; per the design-sync rule (valid design ⇒ no document
  change), no contract document is updated by this triage.
- No ADR amend recommended. Future triggers, if evidence arrives: IC-measured threshold data →
  amend the ADR 0088 constants registry via its benchmark gates; a tiny-tier proposal → new ADR.
- No tests or benchmarks added: there is no Gleaph-side measurable claim in this memo.
