# Payload-first labeled edge traversal

**Status:** Partially Implemented (M1–M6c payload-first predicate expand; sparse-only buckets still combined)

## Purpose

Define a **two-phase** traversal API for labeled edges when callers need edge payloads (weights, timestamps, property bytes). Phase 1 reads **payload bytes only** (and slot metadata). Phase 2 reads **edge rows** only for slots the caller still needs.

This separates *storage IO* from *executor filtering* and avoids materializing full `Edge` values for slots that predicates or indexes already reject.

## Non-goals

- Changing the edge/payload storage layout ([labeled-edge-payloads.md](./labeled-edge-payloads.md)).
- Replacing topology-only traversal (`for_each_edges_for_label_topology_*`) used by hop-count shortest path.
- A single API that always skips edge reads — some workloads (weighted shortest path) still need every live edge’s destination.
- Solving sparse / log-backed buckets in the first milestone (see [Sparse buckets](#sparse-buckets-log-backed)).

## Problem (current behavior)

Today the hot API is `visit_out_edge_payload_batches_for_label`, which delivers `LabeledEdgePayloadBatch { edges, payload_bytes, dense }`.

**Implemented today** (`ic-stable-lara/src/labeled/graph/traverse.rs`):

| Path | IO | Materialization |
|------|----|-----------------|
| **Dense** | Bulk-read edge slab + payload slab in parallel | Parse **every** edge row into `scratch.edges`, zip payloads |
| **Sparse** | Per-slot edge iteration + `attach_edge_payload` | One `Edge` per live slot |

The graph executor’s predicate expand (`expand_candidates_matching_edge_payload_into`) already scans `batch.payload_bytes` first and keeps only matching indices — but LARA has **already** read and parsed all edge rows in the batch. Equality-index expand still walks all edges with payloads and filters by slot in the callback.

Weighted shortest path (`ShortestExpandOptions { load_payloads: true }`) needs **all** live edges (destination + weight). Payload-first does not reduce IO there; regressions on that path are dominated by decode/cache behavior, not edge-vs-payload ordering.

## Invariants (unchanged)

From [labeled-edge-payloads.md](./labeled-edge-payloads.md):

```text
(vertex_id, label_id, edge_slot) → 4 B target in EdgeStore
                              → payload_byte_width bytes in EdgePayloadStore
```

Compaction and scan order stay aligned across edge and payload bytes. Any payload-first batch must report **the same slot order** as the existing combined batch API for the same `(src, label, order)`.

## Proposed API (LARA)

### Phase 1 — payload value batches

```rust
pub struct LabeledPayloadValueBatch<'a> {
    pub label_id: BucketLabelKey,
    pub byte_width: u16,
    pub order: OutEdgeOrder,
    /// Parallel to `values`: absolute edge slot index per chunk.
    pub slot_indices: &'a [u32],
    /// Flattened payload bytes: `slot_indices.len() * byte_width`.
    pub values: &'a [u8],
    pub dense: bool,
}

pub fn visit_out_payload_value_batches_for_label<Visit>(...) -> Result<(), LabeledOperationError>;
```

**Dense bucket** (`payload_log_head < 0 && overflow_log_head < 0 && stored_slots == degree`):

1. Bulk-read `take * byte_width` bytes from `payload_offset + first_slot * width`.
2. Emit `slot_indices = first_slot..first_slot+take` in scan order.
3. Do **not** call `E::read_from` on the edge slab.

Tombstone deletes decrement `degree` without shrinking `stored_slots`, so buckets with in-slab
tombstones are not dense-eligible and use the combined-batch fallback. Phase 2 still skips deleted
slots if eligibility invariants drift.

**Sparse / log-backed:** `visit_out_payload_value_batches_for_label` walks hybrid slab
prefix (bulk payload) plus overflow-log entries, or the sparse span iterator, emitting
`slot_indices` + `values` without retaining edge rows in the batch. Predicate expand uses
phase 1 + phase 2 on these buckets (M6).

Log-backed payload attach uses `LabelBucket::payload_byte_width` to interpret each overflow-log
site: inline bytes in the 8 B log cell when width `<= 8`, else the blob map at `(leaf, entry_idx)`.
Storage class is not stored in the cell ([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md)).

### Phase 2 — selective edge row read

```rust
pub fn read_out_edge_slots_for_label(
    src: VertexId,
    label_id: BucketLabelKey,
    slots: &[u32],
    order: OutEdgeOrder, // defines output order if caller cares
    out: &mut dyn FnMut(E) -> Result<(), LabeledOperationError>,
) -> Result<(), LabeledOperationError>;
```

**Dense:** one `read_slots_contiguous` (or few contiguous spans) for the requested slot set; attach `label_id` + `slot_index`; skip deleted slots.

**Sparse:** read single slots via existing CSR + overflow resolution (same as today, but only for requested indices).

### Combined batch (compatibility)

Dense-eligible buckets keep the **single-pass** parallel edge+payload read in
`visit_dense_out_edge_payload_batches_for_bucket` (bulk `read_slots_contiguous` +
payload bytes). A phase 1+2 adapter was tried and reverted: full-batch callers
(vector expand, high match-rate filters) regressed on canbench versus one IO pass.

Hybrid and sparse buckets keep dedicated combined-batch paths.

## Facade layer (`GraphStore`)

**Crate:** `gleaph-graph` — `facade/store/edge_scan.rs`

| Method | Status | Role |
|--------|--------|------|
| `visit_out_edge_payload_batches_for_label` | Implemented | Combined batch; keep for simple callers |
| `visit_out_payload_value_batches_for_label` | Implemented (dense + hybrid + sparse) | Phase 1 only |
| `read_directed_out_edge_slots_for_label` | Implemented | Phase 2 only |
| `for_each_directed_out_edges_for_label_topology_unchecked` | Implemented | No payload |

Executor routing:

| Workload | Traversal pattern |
|----------|-------------------|
| Hop-count `ShortestPath` | Topology only (`load_payloads: false`) |
| Weighted `ShortestPath` | Bulk payload + bulk edge for **all** live slots (can use phase 1 + phase 2 with full slot list; no filter between phases) |
| `Expand` + payload predicate | Phase 1 → filter → phase 2 on **dense** and **overflow** hubs (M6c + hybrid replay cache) |
| `Expand` + equality index | Index → slot set → phase 2 (payload optional if index key is not payload-derived) |
| `Expand` + vector threshold | Combined batch + filter indices (payload-first deferred — canbench regression at all tested scan/match sizes) |

## Executor integration (planned)

### Predicate expand

Replace “combined batch + filter indices” with:

1. `visit_out_payload_value_batches_for_label`
2. `PreparedEdgePayloadBatchKernel::collect_matching_value_indices(values, …)`
3. `read_directed_out_edge_slots_for_label(&match_slots, …)` → `ExpandDst` + `EdgeBinding`

**Expected win:** proportional to `(1 - match_rate) * degree * E::BYTES` per hub, plus avoided `Edge` struct work on rejected slots.

### Weighted shortest

No change to *which* slots are read. Optional refactor:

1. Phase 1: bulk payload bytes per expand
2. Decode weights with `PreparedWeightDecoder` into a scratch `Vec<WeightedCost>` (no `profile.prepare()` per edge)
3. Phase 2: bulk edge rows, zip with weights in relax

This is an **ordering / decode** optimization, not payload-first filtering.

### Equality index

When postings yield `(label_id, slot_index)` for `src` on **forward** expand:

1. Skip phase 1 (index value already matched)
2. Phase 2 directly: `read_out_edge_slots_for_label(&indexed_slots, …)` per label bucket

Avoids full degree scan. **Reverse / undirected** expand still uses full adjacency scan plus canonical handle matching because postings store forward owner slots.

## Dense eligibility (unchanged)

A bucket is **dense-eligible** for bulk payload read when:

```text
payload_log_head < 0
&& overflow_log_head < 0
&& stored_slots == degree
```

Vertices that fail this (e.g. converging-hub **src** with 48 edges, `stored_slots > degree`) stay on sparse/combined path until sparse payload-first is designed.

## Sparse buckets (log-backed)

**Not in milestone 1.**

Options for later:

- Walk payload log in lockstep with edge overflow iterator (same entry index), emitting value batches without full `Edge` materialization
- Fold-to-slab maintenance to increase dense eligibility on hot hubs (production:
  `GraphStore` edge inserts drain the LARA maintenance queue via
  `post_edge_insert_maintenance_budget`; delete paths drain fully).
  Aggressive vertex-edge-span compaction at batch boundaries is **planned** in
  [bulk-ingest-finalize.md](./bulk-ingest-finalize.md) (not implemented).
- Property / equality index to reduce visited slots without full payload scan

## Migration plan

| Step | Deliverable | Verification |
|------|-------------|--------------|
| M0 | Document + bench scopes (`labeled_visit_payload_value_batches`, `labeled_read_edge_slots`) | canbench pattern runs |
| M1 | Dense `visit_out_payload_value_batches_for_label` | **Implemented** — `values.rs` batch order + parity tests |
| M2 | `read_out_edge_slots_for_label` (dense bulk + sparse/log) | **Implemented** — slot/order parity + phase-1/2 integration test |
| M3 | Facade wrappers + predicate expand switched | **Implemented** — dense + overflow use phase 1+2; sparse keeps combined batch in executor |
| M4 | Equality-index expand uses phase 2 only | **Implemented** — forward (`PointingRight`); reverse/undirected keep full-scan fallback |
| M5 | Weighted shortest: prepared decoder on relax hot path | **Implemented** — `PreparedWeightDecoder::decode`; optional zip refactor deferred |
| M6 | Sparse payload-first | **Implemented (LARA)** — overflow `visit_out_payload_value_batches`; edge-free hybrid slab prefix; cached payload log chains |
| M6a | Executor probe removal | **Implemented** — dense-eligibility pre-check before phase 1 |
| M6b | Edge-free hybrid slab + chain cache | **Implemented** — slab prefix skips edge slab IO; `read_payload_log_chain_entry`; phase-2 overflow chain cache |
| M6c | Executor overflow routing | **Implemented** — edge-free phase-1 tag walk; `HybridOverflowEdgeReplay` caches log table + slot→log_idx for phase 2 (avoids chain rebuild + stable re-read). Naive routing regressed +23% on `expand_payload_skewed_2k`; replay fix **−32%** vs baseline (2026-06-15 canbench) |

**Backward compatibility:** combined `LabeledEdgePayloadBatch` API remains; dense buckets use single-pass bulk read. Phase 1+2 APIs are for selective slot reads (predicate/index expand). Sparse-only paths unchanged in the executor.

### M6c insight: why naive overflow routing regressed

Overflow predicate expand must scan **all** payload bytes to filter (low match rate does not skip payload IO). Combined batch decodes each live edge once while attaching payload. Naive payload-first avoided edge decode in phase 1 but **phase 2 re-built the overflow log chain and re-read stable memory per match slot**, duplicating work. Caching phase-1 replay (`log_table`, `slot_to_log_idx`, slab delete set) makes phase 2 decode only the **m** matching edges from the cached table.

## Benchmark expectations

| Bench | Expectation |
|-------|-------------|
| `expand_payload_skewed_{200a,2k}` | Default `canbench`: single-label hub + `edge_payload_predicate`. M6c replay: **−26%** / **−32%** vs pre-M6c baseline (2026-06-15). |
| `large_expand_payload_skewed_{10k,50k}` | `canbench_large`: heavier graph construction; payload-first vs combined neutral to ~2–4% faster at 50k scan. |
| `expand_skewed_noise_*` | Two-label topology filter (not payload predicate); unchanged by payload-first routing. |
| `weighted_shortest_edge_cost_cache` | **No** gain from payload-first alone; gain from prepared decoder + dense src path |
| `hop_count_shortest_converging_hub` | Unchanged (topology-only) |
| New: `labeled_visit_payload_value_batches` | Isolates phase-1 IO |

## Open questions

1. **Tombstones in dense payload-only scan:** Resolved for dense-eligible buckets — `stored_slots == degree` excludes in-slab tombstones; phase 1 reads payload bytes only. Phase 2 skips deleted slots on invariant drift.
2. **Reverse / in-edges:** Mirror API on in-edge storage; same contract.
3. **Undirected expand:** May require two directed phase-2 reads or a dedicated undirected slot resolver.
4. **ADR:** Not required for M1–M3 (API addition + executor routing). Consider ADR if sparse payload-first changes overflow log scan contract.

## Source of truth

| Layer | Path |
|-------|------|
| LARA traversal | `crates/ic-stable-lara/src/labeled/graph/traverse.rs` |
| Batch types | `crates/ic-stable-lara/src/labeled/graph/iter.rs` |
| GraphStore edge scan | `crates/graph/src/facade/store/edge_scan.rs` |
| Expand routing | `crates/graph/src/plan/query/executor/expand/candidates.rs` |
| Shortest expand | `crates/graph/src/plan/query/executor/path.rs` |
| Weight decode | `crates/graph/src/plan/query/gleaph_weight.rs` |

## Related

- [labeled-edge-payloads.md](./labeled-edge-payloads.md) — storage layout and invariants
- [lara-and-facade.md](./lara-and-facade.md) — GraphStore vs LARA boundaries
- [index/property-index.md](../index/property-index.md) — equality index postings
- [execution/operators.md](../execution/operators.md) — `Expand`, `ShortestPath`
