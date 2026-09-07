# 0094. Tree-bucket tombstone count in the LTB block header (Level 1 OFFSET acceleration)

Date: 2026-09-07
Status: accepted (2026-09-07, management-pane approval; field+maintenance landed Plan 0337
`73ba30e01`, read-path slice pending)
Last revised: 2026-09-07
Anchor timestamp: 2026-09-07 01:28:58 UTC +0000 (OS anchor; §1/§2 status and all file/line
references verified at ADR-draft time 2026-09-07 03:19 UTC +0000)
Amends: [ADR 0088 §1/§2](0088-tree-csr-mode-for-high-degree-label-buckets.md) (block-header
layout; field only — stride, version, and addressing unchanged)
Refines: [ADR 0050](0050-lara-traverse-read-api.md) (traverse read API; window contract per the
Plan 0327 unification), [ADR 0052](0052-per-label-adjacency-order-and-tombstone-reuse.md)
(tombstone semantics unchanged), [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md)
(no-await atomicity), [ADR 0021](0021-resumable-supernode-detach-delete.md) (stepped maintenance)
Resolution path for: GAP-2026-07-25-002 (tombstone-aware OFFSET acceleration — tree side)
Evidence: Plan 0336 (`73ba30e01`-adjacent benches; candidate C rejected), Plan 0337
(`73ba30e01`; Level 1 measurement), research investigation
[2026-09-06-auxiliary-index-redesign-research.md](../investigations/2026-09-06-auxiliary-index-redesign-research.md) §4-§5

## Context

GAP-2026-07-25-002 records that tombstone-heavy OFFSET resolution is materially more expensive
than dense resolution and that no persistent skip structure exists. Plan 0283 rejected persistent
metadata on a query-only crossover; Plan 0336 measured the reactive-compaction candidate C and it
failed its fixed gate at every churn point (one targeted compaction costs 944,957,827
instructions against ~106K/query overshoot); the 2026-09-07 design revision (research doc §5)
reduced candidate A to its minimal form and the byte-slab sidecar variant was rejected on
single-role discipline.

Plan 0337 then produced the first tree-regime OFFSET measurement (1M+1 stored slots, depth-2,
contiguous dead-prefix tombstones):

- The current tree window walk is **offset-insensitive** (~48.2-48.6M instructions at every
  measured point, both densities): `visit_edges_window`'s tree arm feeds every slot of the full
  leaf set through the visitor closure and applies the window cut inside the closure, so the walk
  never restricts itself to the window's block range. K (blocks passed) = leaf count ≈ 1,025 at
  every point — the page-charge-dominated regime the K ≥ 32 criterion exists for.
- A bench-scoped counting walk (root-entry walk + per-block header tombstone count, skipping
  fully-dead blocks without payload reads) measured **52.8× per-query gain** (I_L1/I_D ≈ 1.9%),
  clearing the ε = 0.10 gate at every point, with count parity (per-block header count == scanned
  markers; Σ == `stored_slots − degree`) holding throughout.
- The production header-count field and its funnel maintenance are already landed under the
  pre-deployment fresh-state policy (`73ba30e01`); the production READ path is unchanged.

## Decision

### 1. Block-header field (landed; formalized here)

The LTB block header carries a `u16 tombstone_count` at offset 13-14 (the former 3 reserved
bytes at 13-15 shrink to 1 at offset 15). `BLOCK_HEADER_BYTES` stays 16, `BLOCK_STRIDE` stays
4,112, and **`LAYOUT_VERSION` stays 1**: the canister is not deployed, so the field is simply
added to the current header and used — no version bump, no migration, no compatibility reader,
fresh state per the pre-production policy. `reserved (must be zero)` validation continues to
apply only to the store header, and reopen performs no O(blocks) validation of block headers
(ADR 0088 §8 unchanged).

The count is the block's own accounting — not a second edge-identity structure, not a cache of
any external state. `BucketEntryPosition` identity, `visit_edges` output, and ADR 0052 ordering
semantics are unchanged.

### 2. Maintenance contract (landed; formalized here)

- Mint initializes the count to zero. Every path that mints or rewrites tree blocks either
  inherits a fresh count (compaction rewrite, promote transcription — fresh fully-live blocks)
  or leaves the count correct for the written content (batch `RunDestination::Tree` tail runs
  append live edges — count unchanged). Demotion emits slab buckets (no tree blocks).
- The tree-mode remove funnel (`tree_mode_remove_edge_at_slot`) increments the count inside the
  existing marker → count → descriptor compensation chain; the descriptor-failure rollback
  restores both the marker payload and the previous count; an idempotent re-remove of an
  already-tombstoned slot does not increment.
- The count is maintained by exactly one funnel point; no other marker writer requires a count
  update (audited Plan 0337 list recorded in the research doc).

### 3. Verification contract

The block's marker bytes are the truth; the count is derived state stored beside them.

- **At use:** any walk that enters a block scans its slots and thereby computes the block's true
  tombstone/live counts for free; a scan-computed count that disagrees with the header count is
  block corruption and fails closed as `LabeledOperationError` (ADR 0050 corruption class —
  never a silently wrong window).
- **Bucket-level invariant:** Σ block counts == `stored_slots − degree` is confirmed during walks
  at zero marginal cost.
- **At reopen:** no O(blocks) count validation (ADR 0088 §8 discipline unchanged).

### 4. Production read path (to implement; the new part this ADR adopts)

The tree arm of `visit_edges_window` is restructured to resolve only the blocks overlapping the
window's position range, in two independent sub-mechanisms:

- **S1 — window-restricted block resolution (arithmetic; no new state).** Under the Plan 0327
  pinned contract (the window cuts tombstone-inclusive positions; ascending position == slot
  id; descending position == `extent − 1 − slot`), the start and end blocks of
  `[offset, offset + limit)` are computable by the ADR 0088 §2 addressing arithmetic
  (`i → (root[i / B], i % B)`, depth-generic mixed radix). The walk must not begin at slot 0:
  restricting resolution to the window's block range bounds the tree window cost by the window
  extent instead of the bucket extent. Today's walk-from-slot-0 behavior is an implementation
  gap, not a contract requirement.
- **S2 — in-window dead-block skip (uses the header count).** A block fully inside the window's
  position range whose header count equals its full slot count contains no live rows and is
  skipped without a payload read. This matters for interleaved/aged tombstone distributions
  where fully-dead blocks fall inside the window range; it does nothing for a contiguous dead
  prefix (which S1 already jumps over).

**Attribution requirement:** the Plan 0337 prototype conflated S1 and S2 (its counting walk read
all 1,025 headers, so its 912K includes work S1 would eliminate). The production read-path slice
must measure S0 (current) vs S1 vs S1+S2 separately and record the attribution, and must add a
fixture with fully-dead blocks inside the window range (interleaved distribution) — the 0337
dead-prefix fixtures do not exercise S2's benefit.

**Window-semantics pin:** the Plan 0327 unification fixed the window cut to tombstone-inclusive
positions on every mode (crate-internal consumers only; zero external callers). The
`TraversalWindow` struct documentation still reads "`offset` counts matching live edges" — stale
relative to the pinned contract; the production read-path slice corrects that documentation in
the same patch. The ADR adopts the position-space reading; live-ordinal select is NOT adopted
and remains unnecessary for GQL OFFSET (the upper layers apply row-level paging per ADR 0050's
planner-proof requirements).

### 5. Non-goals and deferred work

- **Slab and bypass regimes are unchanged** (status quo per research doc §5.5): slab extent is
  capped at `T_PROMOTE = 4,096`, worst-case overshoot re-anchored at ~109K instructions/query,
  the compact-or-promote insert-path cycle answers churn, and the deferred slab follow-up
  (512 B bitmap class) stays recorded-not-opened (Plan 0337 slab rule FAILED the 100K bar by
  1.09× — deferred-with-evidence).
- **Level 2 (a dedicated contiguous per-bucket live-count directory region) is not adopted.**
  The measured K distribution confirmed Level 2's shape driver (page charges) is real but Level
  1 already reclaims ~98% of the measured gap; escalation stays gated on future evidence
  (research doc §5.2).
- **Tree-mode tombstone reuse (the ADR 0088 deferred follow-up) is a separate later slice** that
  builds on this count: `count > 0` identifies candidate blocks and a bounded in-block scan
  finds the reusable slot. Folding reuse into this ADR would widen the insert-path surface; the
  count is deliberately designed so reuse can adopt it without a layout change.
- The remove-side maintenance-admission trigger for Insertion-policy slab buckets and bypass
  rows (research doc §5.5) is a separate deferred-worktree item at the deferred-wrapper layer;
  it neither uses nor affects the header count.

## Alternatives considered

1. **Reactive compaction (candidate C)** — rejected by Plan 0336's fixed gate (per-trigger
   compaction 944,957,827 instructions vs ~106K/query overshoot; I_C/I_D = 4.47-4.62×).
2. **Per-bucket sidecar span on the inline-property-bytes byte-slab** — rejected on single-role
   discipline (research doc §5.1): it would give a canonical single-purpose region a second role,
   rewrite the tree-mode zero-fields invariant, and overload descriptor fields.
3. **Level 2 contiguous per-bucket directory region** — deferred behind measured evidence
   (research doc §5.2); Level 1's measured gain makes it unnecessary at the measured scale.
4. **Slab bitmap (512 B class)** — deferred-with-evidence for the slab regime (Plan 0337 slab
   rule); the slab overshoot is bounded by the extent cap and answered by compact-or-promote.
5. **S1 alone without the header count** — viable for the dead-prefix regime by arithmetic
   alone, but it leaves fully-dead blocks inside a window range unskippable without payload
   reads, and it forfeits the tombstone-reuse primitive the deferred follow-up needs. The count
   is 2 bytes per block (0.05% of payload) with a single-funnel maintenance cost; adopted
   together with S1.

## Consequences

Positive: tree window cost becomes bounded by the window extent rather than the bucket extent;
the tombstone-reuse follow-up gains its missing primitive without a `LabelBucket` field;
GAP-2026-07-25-002's tree side has a measured, gate-cleared resolution path; the block's
liveness accounting is local to the block (single funnel, no second structure).

Negative / obligations: the count must be maintained by every tree-block marker writer (audited
list in research doc §5.4); a scan/count mismatch fails closed and is therefore observable;
canbench instruction counts exclude DMT page charges, so production cycle costs include
per-page charges the benches do not (the ADR-level claims are wasm-instruction claims);
`stable-memory-inventory.md` must gain the LTB region rows (graph MemoryId 53/54) and reconcile
the ADR 0088 64-page policy statement against the implemented 16-page policies — recorded as
separate documentation gaps (research doc §5.4), to land with the adoption slice.

## Implementation status

- Field, mint init, remove-funnel increment, parity tests, tree OFFSET baseline, counting-walk
  prototype, slab re-anchor: **landed** (Plan 0337, `73ba30e01`).
- Production read path (S1 + S2), `TraversalWindow` doc correction, S1/S2 attribution
  measurement, in-window dead-block fixture: **this ADR's implementation slice (pending)**.
- Adoption paperwork (GAP-2026-07-25-002 closure, inventory rows): **pending**, after the
  implementation slice's measurements.
