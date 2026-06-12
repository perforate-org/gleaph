# 0007. Stable-memory layout policy and measured consolidation

Date: 2026-06-12  
Status: accepted  
Last revised: 2026-06-12  
Anchor timestamp: 2026-06-12 04:38:55 UTC +0000

## Revision history

| Date | Change |
|------|--------|
| 2026-06-12 | Proposed; baseline layout, separation rules, benchmark-gated consolidation candidates. |
| 2026-06-12 | Accepted; policy frozen at §2 pending §6 benchmarks and registry follow-up. |
| 2026-06-12 | Layout registry in `graph-kernel::stable_layout` + per-canister `layout.rs`. |
| 2026-06-12 | Initial canbench suite in `graph-kernel` (cold touch 5/21/43, router catalog intern). |
| 2026-06-12 | Extended canbench to graph/router/graph-index; §8b preliminary retain/defer judgments. |

## Context

Phases 0–7 of the [refactoring roadmap](../architecture/refactoring-roadmap.md) are complete.
Storage-domain APIs, derived-state rebuild paths, and catalog abstractions are explicit. ADR 0006
repacked graph, router, and graph-index `MemoryId` assignments into consecutive layouts and removed
federation-only stable regions from the single-shard footprint.

The graph canister still allocates **43** `VirtualMemory<DefaultMemoryImpl>` regions (32 LARA + 11
facade). The router allocates **21**; graph-index allocates **5**. Each region carries
`MemoryManager` bookkeeping and encodes an ownership or recovery boundary.

That separation is intentional: canonical adjacency, derived reverse orientation, maintenance free
spans, catalog pairs, telemetry, and postings use different growth rates, access patterns, and
rebuild strategies. A premature merge would weaken corruption isolation and complicate reopen
testing before consolidation benefits are measured.

Phase 8 requires a **layout policy ADR** before any further memory-id or physical-layout change.
This document records the baseline, separation rules, consolidation gate, and non-goals. Concrete
consolidation patches remain **conditional on benchmark evidence** recorded in this ADR or linked
bench results.

### Problems today

| Area | Issue |
|------|--------|
| **Policy gap** | No ADR states when to keep vs merge `VirtualMemory` regions after Phase 0–7 |
| **Layout knowledge** | `MemoryId` constants live in three `memory.rs` files; inventory is prose, not a typed registry |
| **Consolidation pressure** | Many small regions suggest grouping, but hot-path cost of region count is unmeasured |
| **Legacy compatibility** | Graph `EDGE_WEIGHT_PROFILES` (MemoryId 37) remains a separate region though new writes use payload profiles only |
| **Inventory drift** | `INDEX_POSTINGS` rebuild row in inventory understates `backfill_property_postings` coverage |

### Prerequisites (met)

- [stable-memory-inventory.md](../storage/stable-memory-inventory.md) — region classification and rebuild paths
- ADR 0006 — consecutive ids; federation stable removed from single-shard layout
- Phase 2 storage-domain APIs — explicit commit paths per invariant owner
- Phase 3 `BidirectionalCatalog` — router catalog semantics unified at API layer

### Non-goals (this ADR)

- Production data migration or backward compatibility with earlier development snapshots
- LARA bundle internal repack (MemoryIds 0–31 on graph) without a dedicated LARA layout ADR
- Merging canonical graph state with derived postings (graph-index remains a separate canister)
- Federation reintroduction (`RemoteVertexId` stable, peer principals) — separate follow-up ADR
- Monolithic single-region stable memory for an entire canister
- Bumping crate or schema version numbers solely to denote dev-only layout changes

---

## Decision

### 1. Hybrid VirtualMemory policy

**Keep the default:** one stable region per distinct ownership, growth, access, or rebuild
boundary. Do **not** consolidate regions early to reduce `MemoryId` count alone.

**Allow consolidation only when all of the following hold:**

1. Domain APIs for the affected stores are explicit (Phase 2 met).
2. Benchmark evidence shows a measurable win on a relevant hot path (init, reopen, catalog intern,
   admin step, or documented query/mutation path).
3. Corruption isolation impact is documented (see §5).
4. Reopen or upgrade tests cover the new layout.
5. [stable-memory-inventory.md](../storage/stable-memory-inventory.md) and this ADR are updated in
   the same patch.

If benchmarks show no win, **retain current separation**. That is a valid Phase 8 outcome.

### 2. Baseline layout (2026-06-12)

Code source of truth:

- `crates/graph/src/facade/stable/memory.rs`
- `crates/router/src/facade/stable/memory.rs`
- `crates/graph-index/src/facade/stable/memory.rs`

| Canister | Region count | Id range | Notes |
|----------|-------------|----------|-------|
| Graph — LARA bundle | 32 | 0–31 | Forward canonical + reverse derived + maintenance; wired into one `DeferredBidirectionalLabeledLaraGraph` |
| Graph — facade | 11 | 32–42 | Properties, labels, aliases, profiles, local indexes, telemetry, idempotency |
| Router | 21 | 0–20 | Registry, placement, catalogs (3 pairs), auth, telemetry (4), idempotency (2), backfill cursors (2) |
| Graph-index | 5 | 0–4 | Admins, shard owners, property postings, router auth, label postings |

Ephemeral heap state (pending posting queues, router planner catalog, prepared plans) is **not**
part of this layout; see inventory § ephemeral.

### 3. Regions that must stay separated

Do not merge across these boundaries without a new ADR and benchmark proof:

| Boundary | Rationale |
|----------|-----------|
| Forward vs reverse LARA orientation | Canonical vs derived; different rebuild story |
| Edge slab vs payload slab / log / blobs | Different access patterns and compaction |
| LARA adjacency vs graph facade (32+) | `ic-stable-lara` bundle vs Gleaph domain stores |
| Vertex/edge property values vs graph-index postings | Separate canisters; postings are derived |
| Graph-index property postings vs label postings | Different key shapes and backfill paths |
| Router placement vs label telemetry | Canonical placement vs event-derived aggregates |
| Canonical stores vs maintenance queues | Query scan paths must not depend on PMA maintenance |
| Router catalog vs telemetry vs backfill cursors | Different lifecycles and admin surfaces |

Within LARA, **free-span store + by-start index pairs** (e.g. ids 2–3, 8–9) remain paired regions.
Merging a pair into one region is **out of scope** unless `ic-stable-lara` publishes a layout ADR.

### 4. Consolidation candidates (benchmark-gated)

These are **allowed to prototype** after §6 benchmarks. None are approved by this ADR alone.

| Priority | Candidate | Current ids | Hypothesis | Gate |
|----------|-----------|-------------|------------|------|
| P1 | Retire `EDGE_WEIGHT_PROFILES` | Graph 37 | Legacy read fallback only; new installs write payload profiles (Phase 3) | Prove no production reliance; reopen test without region 37 |
| P2 | Router catalog VM grouping | Router 5–10 (3 pairs) | Pair maps always updated together via `BidirectionalCatalog` | Measure intern + reopen vs 3 grouped metadata regions |
| P3 | Label telemetry seq + outbox | Graph 40–41 | Same mutation pipeline | Measure DML + reopen; assess telemetry replay isolation |
| P4 | Router backfill cursors | Router 19–20 | Same admin API surface | Low priority; measure admin step latency only |

**Not candidates** without new evidence: LARA 0–31 repack, forward/reverse merge, property +
label posting merge on graph-index, router telemetry merge into placement.

### 5. Corruption isolation and reopen

| Policy | Choice |
|--------|--------|
| Development data | **Intentionally discarded** on layout change; reinstall or wipe stable memory |
| Production migration | Not in scope until a product migration ADR exists |
| Reopen tests | Required for any id reassignment or region merge; use existing facade reopen tests per canister |
| Partial corruption | Separate regions limit blast radius; merged regions must document shared fate |
| Upgrade | Ephemeral queues (pending postings, planner catalog) lost on upgrade by design; backfill covers historical derived state |

### 6. Benchmark plan (Phase 8 deliverable)

Record results in this ADR (revision history) or linked **canbench** output (`crates/graph-kernel/canbench_results.yml`).
Internet Computer–facing stable-memory paths use **canbench on wasm32**, not native criterion.
Until consolidation rows exist, **no consolidation patch merges**.

| Benchmark | Purpose | Status |
|-----------|---------|--------|
| `bench_layout_memory_manager_cold_touch_{5,21,43}` | VM count overhead at cold start (graph-index / router / graph region counts) | **Done** |
| `bench_layout_graph_stable_reopen_touch` | Graph facade re-init on persisted memory manager | **Done** |
| `bench_layout_router_stable_reopen_touch` | Router facade re-init | **Done** |
| `bench_layout_router_three_catalog_intern_6vm` | P2 baseline (six-region router catalog layout) | **Done** — grouped prototype TBD |
| `bench_layout_edge_weight_profile_{payload_only,with_legacy_fallback}` | P1 sunset evidence | **Done** |
| `bench_layout_index_posting_insert_64` | Posting insert hot path (backfill proxy) | **Done** |
| Grouped catalog prototype vs 6 VM | P2 merge gate | **Pending** |

**Results (2026-06-12, `gleaph-graph-kernel` canbench, wasm32):**

| Bench | Regions | Instructions | Stable memory Δ (pages) |
|-------|---------|--------------|---------------------------|
| `bench_layout_memory_manager_cold_touch_5` | 5 | 127.81 K | 641 |
| `bench_layout_memory_manager_cold_touch_21` | 21 | 332.17 K | 2,689 |
| `bench_layout_memory_manager_cold_touch_43` | 43 | 613.15 K | 5,505 |
| `bench_layout_router_three_catalog_intern_6vm` | 6 (catalog) | 11.81 M | 769 |
| `bench_layout_graph_stable_reopen_touch` | 43 (facade+LARA) | 507.15 K | 6,144 |
| `bench_layout_router_stable_reopen_touch` | 21 | 41.91 K | 256 |
| `bench_layout_edge_weight_profile_payload_only` | — | 193.21 K | 0 |
| `bench_layout_edge_weight_profile_with_legacy_fallback` | — | 193.21 K | 0 |
| `bench_layout_index_posting_insert_64` | 1 posting set | 3.44 M | 0 |

**Reading:** cold `MemoryManager` + one empty `BTreeMap` insert per region scales roughly linearly
(+~14 K instructions and +~128 pages per region in this synthetic probe). That supports keeping
separation unless a **hot-path** benchmark shows consolidation wins. Router catalog intern cost is
dominated by map inserts (~12 M instructions for 96 intern ops), not region count alone — P2 needs a
grouped-layout prototype bench before merge.

Existing hot-path benches (labeled hub insert/scan, router seed dispatch) remain regression guards;
they do not substitute for layout-specific measurements.

### 8b. Consolidation judgment (2026-06-12)

Preliminary decisions from §6 canbench (wasm32). Final after grouped-catalog prototype if pursued.

| ID | Decision | Rationale |
|----|----------|-----------|
| P1 `EDGE_WEIGHT_PROFILES` | **Defer retire** | Payload-only and legacy-fallback reads tie at ~193 K instructions; removal needs legacy-stable absence proof + reopen tests, not read-path savings alone |
| P2 Router catalog grouping | **Retain** | Intern dominated by map work (~12 M ins); no grouped prototype bench yet |
| P3 Label telemetry merge | **Retain** | Not measured; isolation outweighs unproven VM savings |
| P4 Backfill cursors | **Retain** | Low priority; no hot-path evidence |

**Phase 8 close (8d):** 8a complete except grouped-catalog prototype (optional). 8c consolidation
**not required** for Phase 8 close at this time.

### 7. Memory layout registry (Phase 8 deliverable)

Introduce a **named layout registry** that mirrors inventory rows without changing runtime behavior
in the first patch:

- Shared descriptor types in `graph-kernel` or per-crate `facade/stable/layout.rs`
- Each region: symbol, `MemoryId`, class (canonical / derived / maintenance / catalog / telemetry /
  compatibility), owner domain, rebuild fn name (if any)
- `memory.rs` constants remain the runtime source of ids; registry is documentation + test aid
  until a layout change requires codegen or validation

Registry lands in a follow-up patch after this ADR is accepted. **Done (2026-06-12):**
`gleaph_graph_kernel::stable_layout` plus `facade/stable/layout.rs` in graph, router, and
graph-index; runtime ids remain in each `memory.rs`.

### 8. Change procedure

Any patch that changes `MemoryId` assignment or merges regions must:

1. Update this ADR (before/after table) or supersede with a child layout ADR.
2. Update [stable-memory-inventory.md](../storage/stable-memory-inventory.md).
3. Add or extend reopen tests.
4. Attach benchmark delta or explicit “no measurable change” note.
5. Run focused crate tests and relevant canbench per [rust-workflow](../../.skills/rust-workflow/SKILL.md).

---

## Consequences

### Positive

- Explicit gate prevents speculative merges that weaken isolation.
- Baseline layout is ADR-backed for Phase 8 and future federation stable reintroduction.
- “No consolidation” remains a documented, evidence-based outcome.
- Registry path reduces inventory ↔ code drift.

### Negative / cost

- 43 + 21 + 5 regions retain manager overhead until benchmarks prove otherwise.
- P1 weight-profile retirement requires confirming legacy stable read paths in tests/benches.
- Two-step delivery: policy ADR now, registry and benchmarks before code layout changes.

---

## Alternatives considered

| Alternative | Why rejected |
|-------------|--------------|
| Merge all router catalogs into one region now | No benchmark; breaks per-catalog corruption isolation |
| Monolithic graph stable region | Conflicts with LARA bundle wiring and derived/canonical split |
| Defer ADR until after consolidation | Roadmap requires ADR before layout changes; risks unreviewed merges |
| Mandatory consolidation to reduce id count | Id count alone is not a measured problem |
| Full production migration tooling | Out of scope for active development; dev reinstall acceptable per roadmap |

---

## Implementation order

1. ~~**Accept this ADR**~~ — layout policy and baseline frozen at §2. **Done (2026-06-12).**
2. **Fix inventory drift** — `INDEX_POSTINGS` rebuild row; link this ADR from inventory.
3. ~~**Add layout registry**~~ — descriptive only; no id changes. **Done (2026-06-12).**
4. ~~**Run §6 benchmarks**~~ — **Done (2026-06-12)** except optional grouped-catalog prototype.
5. **Optional consolidation patches** — none required per §8b; revisit P1 when legacy stable is absent.

---

## References

- [Refactoring roadmap Phase 8](../architecture/refactoring-roadmap.md)
- [Stable-memory inventory](../storage/stable-memory-inventory.md)
- [ADR 0006 — Pre-federation foundation](0006-pre-federation-foundation.md)
- [Derived-state query semantics](../index/derived-state-query-semantics.md)
- [LARA and graph facade](../storage/lara-and-facade.md)
