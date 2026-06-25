# 0007. Stable-memory layout policy and measured consolidation

Date: 2026-06-12
Status: accepted
Last revised: 2026-06-25
Anchor timestamp: 2026-06-15 11:41:23 UTC +0000

## Revision history

| Date       | Change                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 2026-06-12 | Proposed; baseline layout, separation rules, benchmark-gated consolidation candidates.                                                                                                                                                                                                                                                                                                                                    |
| 2026-06-12 | Accepted; policy frozen at §2 pending §6 benchmarks and registry follow-up.                                                                                                                                                                                                                                                                                                                                               |
| 2026-06-12 | Layout registry in `graph-kernel::stable_layout` + per-canister `layout.rs`.                                                                                                                                                                                                                                                                                                                                              |
| 2026-06-12 | Initial canbench suite in `graph-kernel` (cold touch 5/21/43, router catalog intern).                                                                                                                                                                                                                                                                                                                                     |
| 2026-06-17 | Router compact: retired controllers + placement slots; auth at 0; **34** regions (0–33).                                                                                                                                                                                                                                                                                                                                  |
| 2026-06-24 | Router catalog growth recorded: ADR 0030 appended constraint catalog + reservation table + reverse index + pending-effect discovery (34→40); ADR 0031 Slice 3 appended the embedding-name catalog (40–41) + derived vector-index definition catalog (42), reaching **43** regions (0–42).                                                                                                                                 |
| 2026-06-15 | **Phase 8 closed (8d):** 8a complete; 8b final (P2/P4 retain, P1/P3 done); 8c not required; grouped-catalog prototype not pursued.                                                                                                                                                                                                                                                                                        |
| 2026-06-12 | Extended canbench to graph/router/graph-index; §8b preliminary retain/defer judgments.                                                                                                                                                                                                                                                                                                                                    |
| 2026-06-12 | P1 executed: retired `EDGE_WEIGHT_PROFILES`; graph facade repacked to 42 regions (ids 37–41).                                                                                                                                                                                                                                                                                                                             |
| 2026-06-12 | ADR 0008 executed: retired graph `EDGE_PAYLOAD_PROFILES`; graph 41 regions (facade 32–40); router 22 regions (0–21).                                                                                                                                                                                                                                                                                                      |
| 2026-06-12 | ADR 0009 phase D: retired graph `EDGE_EQUALITY_POSTINGS`; graph 40 regions (facade 32–39).                                                                                                                                                                                                                                                                                                                                |
| 2026-06-12 | canbench: `cold_touch_40` 574.83 K / 5,121 pages; graph stable reopen 487.30 K / 5,760 pages (post-0009).                                                                                                                                                                                                                                                                                                                 |
| 2026-06-12 | ADR 0009 follow-up: index catalog row layout — `ROUTER_NAMED_INDEXES` (22) + `ROUTER_INDEXED_PROPERTY_SET` (23); router **24** regions (0–23).                                                                                                                                                                                                                                                                            |
| 2026-06-23 | ADR 0031 slice 1: added canonical graph `VERTEX_EMBEDDINGS` (44); graph **45** regions (facade 32–44).                                                                                                                                                                                                                                                                                                                    |
| 2026-06-23 | ADR 0031 slice 2: new `graph-vector-index` canister; `VECTOR_INDEX_STABLE_LAYOUT` **11** regions (0–10); MemoryId 6 (`IVF_CENTROIDS`) reserved-empty for Slice 4.                                                                                                                                                                                                                                                         |
| 2026-06-24 | ADR 0031 slice 4: added canonical graph `VERTEX_EMBEDDING_INCARNATIONS` (45) for the delete-spanning incarnation fence; graph **46** regions (facade 32–45). Added Router `ROUTER_VECTOR_DISPATCH_ACTIVATION` (43) global activation flag; router **44** regions (0–43).                                                                                                                                                  |
| 2026-06-24 | ADR 0031 slice 6: added `graph-vector-index` `VECTOR_ID_TO_SUBJECT` (11), a derived `(index_id, vector_id) → VectorSubject` reverse locator for the partition-page search path; `VECTOR_INDEX_STABLE_LAYOUT` **12** regions (0–11).                                                                                                                                                                                       |
| 2026-06-24 | ADR 0031 slice 7: added `graph-vector-index` `VECTOR_REBUILD_STATE` (12), a derived per-index bounded shadow-version rebuild lifecycle; `SubjectMapEntry` gained `shadow_slot` (serde-default, no repack) for atomic publish; `VECTOR_INDEX_STABLE_LAYOUT` **13** regions (0–12).                                                                                                                                         |
| 2026-06-25 | ADR 0031 slice 10: added `graph-vector-index` `VECTOR_MAINTENANCE_STATE` (14), a maintenance/operational execution state for per-index page-health scans (persists across upgrade, cleared only on init/reset); `VECTOR_INDEX_STABLE_LAYOUT` **15** regions (0–14). Added Router `ROUTER_VECTOR_MAINTENANCE_POLICIES` (44), the Router-owned maintenance policy catalog (default disabled); router **45** regions (0–44). |
| 2026-06-25 | Corrected `VECTOR_MAINTENANCE_STATE` classification in the revision history: it is `StableMemoryClass::Maintenance` with `RebuildPath::None`, not derived.                                                                                                                                                                                                                                                                |
| 2026-06-25 | Marked the Context region-count paragraph as historical background and directed readers to the typed registry and §2 summary table for current counts. |

## Context

Phases 0–8 of the [refactoring roadmap](../architecture/refactoring-roadmap.md) are complete.
Storage-domain APIs, derived-state rebuild paths, and catalog abstractions are explicit. ADR 0006
repacked graph, router, and graph-index `MemoryId` assignments into consecutive layouts and removed
federation-only stable regions from the single-shard footprint.

At the time ADR 0007 was accepted, the graph canister allocated **41** `VirtualMemory<DefaultMemoryImpl>`
regions (32 LARA + 9 facade), the router **24**, and graph-index **5**. Each region carries
`MemoryManager` bookkeeping and encodes an ownership or recovery boundary. Current counts are
governed by the typed registry in `crates/graph-kernel/src/stable_layout.rs` and the summary table
in §2 below.

That separation is intentional: canonical adjacency, derived reverse orientation, maintenance free
spans, catalog pairs, telemetry, and postings use different growth rates, access patterns, and
rebuild strategies. A premature merge would weaken corruption isolation and complicate reopen
testing before consolidation benefits are measured.

Phase 8 requires a **layout policy ADR** before any further memory-id or physical-layout change.
This document records the baseline, separation rules, consolidation gate, and non-goals. Concrete
consolidation patches remain **conditional on benchmark evidence** recorded in this ADR or linked
bench results.

### Problems today

| Area                       | Issue                                                                                                     |
| -------------------------- | --------------------------------------------------------------------------------------------------------- |
| **Policy gap**             | No ADR states when to keep vs merge `VirtualMemory` regions after Phase 0–7                               |
| **Layout knowledge**       | `MemoryId` constants live in three `memory.rs` files; inventory is prose, not a typed registry            |
| **Consolidation pressure** | Many small regions suggest grouping, but hot-path cost of region count is unmeasured                      |
| **Inventory drift**        | `INDEX_VERTEX_POSTINGS` rebuild row in inventory understates `backfill_vertex_property_postings` coverage |

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

| Canister            | Region count | Id range | Notes                                                                                                                                                                                                                                                                                                                                                                                   |
| ------------------- | ------------ | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Graph — LARA bundle | 32           | 0–31     | Forward canonical + reverse derived + maintenance; wired into one `DeferredBidirectionalLabeledLaraGraph`                                                                                                                                                                                                                                                                               |
| Graph — facade      | 8            | 32–39    | Properties, labels, aliases, label stats delta log, mutation journal                                                                                                                                                                                                                                                                                                                    |
| Router              | 45           | 0–44     | Grouped auth → registry → runtime config → idempotency → catalog → telemetry → maintenance → constraint catalog/reservations (ADR 0030, 34–39) → embedding-name catalog + vector-index defs (ADR 0031 Slice 3, 40–42) → vector dispatch activation flag (ADR 0031 Slice 4, 43) → vector maintenance policy catalog (ADR 0031 Slice 10, 44); `ROUTER_GRAPH_RUNTIME_CONFIG` at MemoryId 5 |
| Graph-index         | 7            | 0–6      | Router auth, shard catalog, ownership config, then derived postings                                                                                                                                                                                                                                                                                                                     |
| Graph-vector-index  | 15           | 0–14     | Router auth, shard catalog, ownership config, index defs + allocators, centroid meta, reserved centroids, subject clock, id→slot, partition heads, page meta (ADR 0031 Slice 2 / ADR 0032), id→subject reverse locator (ADR 0031 Slice 6), rebuild lifecycle state (ADR 0031 Slice 7), row slab (ADR 0032), maintenance scan state (ADR 0031 Slice 10)                                  |

Ephemeral heap state (pending posting queues on graph canisters) is **not**
part of this layout; see inventory § ephemeral. Router prepared plans are **stable**
(`ROUTER_PREPARED_PLANS`, MemoryId 8) as of 2026-06-17 compact.

### 3. Regions that must stay separated

Do not merge across these boundaries without a new ADR and benchmark proof:

| Boundary                                                              | Rationale                                                                                           |
| --------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| Forward vs reverse LARA orientation                                   | Canonical vs derived; different rebuild story                                                       |
| Edge slab vs payload slab / log / blobs                               | Different access patterns and compaction                                                            |
| LARA adjacency vs graph facade (32+)                                  | `ic-stable-lara` bundle vs Gleaph domain stores                                                     |
| Vertex/edge property values vs graph-index postings                   | Separate canisters; postings are derived                                                            |
| Graph-index property postings vs label postings                       | Different key shapes and backfill paths                                                             |
| Router placement vs label stats projection                            | _(removed)_ Router placement retired ADR 0017; label stats projection remains separate from catalog |
| Canonical stores vs maintenance queues                                | Query scan paths must not depend on PMA maintenance                                                 |
| Router catalog vs label stats / projection cursor vs backfill cursors | Different lifecycles and admin surfaces                                                             |

Within LARA, **free-span store + by-start index pairs** (e.g. ids 2–3, 8–9) remain paired regions.
Merging a pair into one region is **out of scope** unless `ic-stable-lara` publishes a layout ADR.

### 4. Consolidation candidates (benchmark-gated)

These are **allowed to prototype** after §6 benchmarks. None are approved by this ADR alone.

| Priority | Candidate                     | Current ids           | Hypothesis                                                   | Gate                                                                                                                                          |
| -------- | ----------------------------- | --------------------- | ------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| P1       | Retire `EDGE_WEIGHT_PROFILES` | ~~Graph 37~~          | ~~Legacy read fallback~~                                     | **Done** (2026-06-12) — dev data discard; payload profiles only at id 37                                                                      |
| P2       | Router catalog VM grouping    | Router 5–10 (3 pairs) | Pair maps always updated together via `BidirectionalCatalog` | Measure intern + reopen vs 3 grouped metadata regions                                                                                         |
| P3       | Label stats delta seq + log   | Graph 37–38           | Same mutation pipeline                                       | **Done** (2026-06-15) — repacked per [ADR 0015](0015-label-stats-projection-log.md); router dedup set removed, projection cursor at router 17 |
| P4       | Router backfill cursors       | Router 19–20          | Same admin API surface                                       | Low priority; measure admin step latency only                                                                                                 |

**Not candidates** without new evidence: LARA 0–31 repack, forward/reverse merge, property +
label posting merge on graph-index, router label stats merge into placement.

### 5. Corruption isolation and reopen

| Policy               | Choice                                                                                                                                                    |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Development data     | **Intentionally discarded** on layout change; reinstall or wipe stable memory                                                                             |
| Production migration | Not in scope until a product migration ADR exists                                                                                                         |
| Reopen tests         | Required for any id reassignment or region merge; use existing facade reopen tests per canister                                                           |
| Partial corruption   | Separate regions limit blast radius; merged regions must document shared fate                                                                             |
| Upgrade              | Ephemeral queues (pending postings, prepared plans) lost on upgrade by design; backfill covers historical derived state; indexed catalog survives upgrade |

### 6. Benchmark plan (Phase 8 deliverable)

Record results in this ADR (revision history) or linked **canbench** output (`crates/graph-kernel/canbench_results.yml`).
Internet Computer–facing stable-memory paths use **canbench on wasm32**, not native criterion.
Until consolidation rows exist, **no consolidation patch merges**.

| Benchmark                                          | Purpose                                                                      | Status                                             |
| -------------------------------------------------- | ---------------------------------------------------------------------------- | -------------------------------------------------- |
| `bench_layout_memory_manager_cold_touch_{5,21,41}` | VM count overhead at cold start (graph-index / router / graph region counts) | **Done**                                           |
| `bench_layout_graph_stable_reopen_touch`           | Graph facade re-init on persisted memory manager                             | **Done**                                           |
| `bench_layout_router_stable_reopen_touch`          | Router facade re-init                                                        | **Done**                                           |
| `bench_layout_router_three_catalog_intern_6vm`     | P2 baseline (six-region router catalog layout)                               | **Done** — grouped prototype TBD                   |
| `bench_layout_edge_weight_profile_read`            | Edge profile read via `GraphStore`                                           | **Done** (post-P1)                                 |
| `bench_layout_index_posting_insert_64`             | Posting insert hot path (backfill proxy)                                     | **Done**                                           |
| Grouped catalog prototype vs 6 VM                  | P2 merge gate                                                                | **N/A** (retain without prototype; Phase 8 closed) |

**Results (2026-06-12, layout canbench, wasm32):**

| Bench                                                              | Regions                | Instructions | Stable memory Δ (pages) |
| ------------------------------------------------------------------ | ---------------------- | ------------ | ----------------------- |
| `bench_layout_memory_manager_cold_touch_5`                         | 5 (graph-index)        | 140.59 K     | 769                     |
| `bench_layout_memory_manager_cold_touch_21`                        | 22 (router, post-0008) | 344.94 K     | 2,817                   |
| `bench_layout_memory_manager_cold_touch_40`                        | 40 (graph, post-0009)  | 574.83 K     | 5,121                   |
| `bench_layout_memory_manager_cold_touch_41`                        | 41 (graph, post-0008)  | 587.61 K     | 5,249                   |
| `bench_layout_memory_manager_cold_touch_42`                        | 42 (pre-0008)          | 600.38 K     | 5,377                   |
| `bench_layout_memory_manager_cold_touch_43`                        | 43 (pre-P1)            | 613.15 K     | 5,505                   |
| `bench_layout_router_three_catalog_intern_6vm`                     | 6 (catalog)            | 11.81 M      | 769                     |
| `bench_layout_graph_stable_reopen_touch`                           | 40 (post-0009)         | 487.30 K     | 5,760                   |
| `bench_layout_graph_stable_reopen_touch` (post-0008)               | 41                     | 494.08 K     | 5,888                   |
| `bench_layout_graph_stable_reopen_touch` (pre-0008)                | 42                     | 502.05 K     | 6,016                   |
| `bench_layout_router_stable_reopen_touch`                          | 22 (post-0008)         | 49.21 K      | 384                     |
| `bench_layout_router_stable_reopen_touch` (pre-0008)               | 21                     | 41.91 K      | 256                     |
| `bench_layout_edge_weight_profile_read` (post-0008, test registry) | —                      | 2.16 K       | 0                       |
| `bench_layout_index_posting_insert_64`                             | 1 posting set          | 3.44 M       | 0                       |

**Reading:** cold `MemoryManager` + one empty `BTreeMap` insert per region scales roughly linearly
(+~14 K instructions and +~128 pages per region in this synthetic probe). That supports keeping
separation unless a **hot-path** benchmark shows consolidation wins. Router catalog intern cost is
dominated by map inserts (~12 M instructions for 96 intern ops), not region count alone — P2 needs a
grouped-layout prototype bench before merge.

Existing hot-path benches (labeled hub insert/scan, router seed dispatch) remain regression guards;
they do not substitute for layout-specific measurements.

### 8b. Consolidation judgment (final 2026-06-15)

Decisions from §6 canbench (wasm32). Grouped-catalog prototype was not pursued; P2 retain is final.

| ID                         | Decision    | Rationale                                                                                                           |
| -------------------------- | ----------- | ------------------------------------------------------------------------------------------------------------------- |
| P1 `EDGE_WEIGHT_PROFILES`  | **Retired** | Backward compatibility not required (roadmap dev policy); separate stable region removed; facade ids 37–41 repacked |
| P2 Router catalog grouping | **Retain**  | Intern dominated by map work (~12 M ins); no grouped prototype bench yet                                            |
| P3 Label stats delta log   | **Done**    | Repacked 2026-06-15 per ADR 0015                                                                                    |
| P4 Backfill cursors        | **Retain**  | Low priority; no hot-path evidence                                                                                  |

**Phase 8 close (8d): Closed 2026-06-15.** 8a complete (grouped-catalog prototype N/A). 8c consolidation
**not required**. Future layout changes require a new ADR per §2.

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

- 40 + 34 + 7 regions (graph + router + graph-index) retain manager overhead until benchmarks prove otherwise.
- P1 weight-profile retirement requires confirming legacy stable read paths in tests/benches.
- Two-step delivery: policy ADR now, registry and benchmarks before code layout changes.

---

## Alternatives considered

| Alternative                                   | Why rejected                                                              |
| --------------------------------------------- | ------------------------------------------------------------------------- |
| Merge all router catalogs into one region now | No benchmark; breaks per-catalog corruption isolation                     |
| Monolithic graph stable region                | Conflicts with LARA bundle wiring and derived/canonical split             |
| Defer ADR until after consolidation           | Roadmap requires ADR before layout changes; risks unreviewed merges       |
| Mandatory consolidation to reduce id count    | Id count alone is not a measured problem                                  |
| Full production migration tooling             | Out of scope for active development; dev reinstall acceptable per roadmap |

---

## Implementation order

1. ~~**Accept this ADR**~~ — layout policy and baseline frozen at §2. **Done (2026-06-12).**
2. **Fix inventory drift** — `INDEX_VERTEX_POSTINGS` rebuild row; link this ADR from inventory.
3. ~~**Add layout registry**~~ — descriptive only; no id changes. **Done (2026-06-12).**
4. ~~**Run §6 benchmarks**~~ — **Done (2026-06-12)**; grouped-catalog prototype **not pursued** (P2 retain final).
5. ~~**Optional consolidation patches**~~ — **N/A** per §8b; Phase 8 closed 2026-06-15.

---

## References

- [Refactoring roadmap Phase 8](../architecture/refactoring-roadmap.md)
- [Stable-memory inventory](../storage/stable-memory-inventory.md)
- [ADR 0006 — Pre-federation foundation](0006-pre-federation-foundation.md)
- [Derived-state query semantics](../index/derived-state-query-semantics.md)
- [LARA and graph facade](../storage/lara-and-facade.md)
