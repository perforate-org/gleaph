# Inline-property-first labeled edge traversal

**Status:** Partially Implemented (M1–M6c inline-property-bytes-first predicate expand; sparse-only buckets still combined)

**Mechanism note (2026-11-09):** the shared per-leaf overflow log was retired by
[ADR 0097](../adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md). Inline-property
bytes beyond a bucket's slab prefix now live in that bucket's **values spill run**: rows are
addressed by live ordinal, and wide bodies are blobs keyed by `(run, row)`. Statements and
measurements below that name log chains, log entry indices or phase-2 chain caches (notably
M2/M6/M6b/M6c) are **historical**: they record the retired mechanism they were measured under and
are not the live contract.

## Purpose

Define a **two-phase** traversal API for labeled edges when callers need edge inline properties (weights, timestamps, property bytes). Phase 1 reads **inline property bytes only** (and slot metadata). Phase 2 reads **edge rows** only for slots the caller still needs.

This separates *storage IO* from *executor filtering* and avoids materializing full `Edge` values for slots that predicates or indexes already reject.

## Non-goals

- Changing the edge/inline property bytes storage layout ([labeled-edge-inline-properties.md](./labeled-edge-inline-properties.md)).
- Replacing topology-only traversal (`for_each_edges_for_label_topology_*`) used by hop-count shortest path.
- A single API that always skips edge reads — some workloads (weighted shortest path) still need every live edge’s destination.
- Solving sparse / run-backed buckets in the first milestone (see [Sparse buckets](#sparse-buckets-run-backed)).

## Problem (current behavior)

Today the hot API is `visit_out_edge_inline_property_batches_for_label`, which delivers `LabeledEdgeInlinePropertyBatch { edges, inline_property_bytes, dense }`.

**Implemented today** (`ic-stable-lara/src/labeled/graph/traverse.rs`):

| Path | IO | Materialization |
|------|----|-----------------|
| **Dense** | Bulk-read edge slab + inline property bytes slab in parallel | Parse **every** edge row into `scratch.edges`, zip payloads |
| **Sparse** | Per-slot edge iteration + `attach_edge_inline_property` | One `Edge` per live slot |

The graph executor’s predicate expand (`expand_candidates_matching_edge_inline_property_into`) already scans `batch.inline_property_bytes` first and keeps only matching indices — but LARA has **already** read and parsed all edge rows in the batch. Equality-index expand still walks all edges with payloads and filters by slot in the callback.

Weighted shortest path (`ShortestExpandOptions { load_payloads: true }`) needs **all** live edges (destination + weight). Inline-property-first does not reduce IO there; regressions on that path are dominated by decode/cache behavior, not edge-vs-inline property bytes ordering.

## Invariants (unchanged)

From [labeled-edge-inline-properties.md](./labeled-edge-inline-properties.md):

```text
(vertex_id, label_id, edge_slot) → 4 B target in EdgeStore
                              → inline_property_byte_width bytes in EdgeInlinePropertyBytesStore
```

Compaction and scan order stay aligned across edge and inline property bytes. Any inline-property-bytes-first batch must report **the same slot order** as the existing combined batch API for the same `(src, label, order)`.

## Proposed API (LARA)

### Phase 1 — inline property bytes value batches

```rust
pub struct LabeledPayloadValueBatch<'a> {
    pub label_id: BucketLabelKey,
    pub byte_width: u16,
    pub order: OutEdgeOrder,
    /// Parallel to `values`: absolute edge slot index per chunk.
    pub slot_indices: &'a [u32],
    /// Flattened inline property bytes: `slot_indices.len() * byte_width`.
    pub values: &'a [u8],
    pub dense: bool,
}

pub fn visit_out_payload_value_batches_for_label<Visit>(...) -> Result<(), LabeledOperationError>;
```

**Dense bucket** (`bucket_dense_slab_inline_property_bytes_readable`: `is_inline_property_bytes_allocated() && inline_property_byte_width > 0 && inline_property_bytes_log_len == 0 && inline_property_bytes_slab_slots == degree`):

1. Bulk-read `take * byte_width` bytes from `payload_offset + first_slot * width`.
2. Emit `slot_indices = first_slot..first_slot+take` in scan order.
3. Do **not** call `E::read_from` on the edge slab.

Tombstone deletes decrement `degree` without shrinking `stored_slots`, so buckets with in-slab
tombstones are not dense-eligible and use the combined-batch fallback. Phase 2 still skips deleted
slots if eligibility invariants drift.

**Sparse / run-backed:** `visit_out_payload_value_batches_for_label` walks the hybrid slab
prefix (bulk inline property bytes) plus the bucket's values-run rows, or the sparse span
iterator, emitting `slot_indices` + `values` without retaining edge rows in the batch. Predicate
expand uses phase 1 + phase 2 on these buckets (M6).

Sparse inline property bytes attach uses `LabelBucket::inline_property_byte_width` to interpret
each values-run row: inline bytes in the 8 B cell when width `<= 8`, else the blob map at
`(run, row)`. Storage class is not stored in the cell. Values-run rows whose live ordinal is
skipped by edge replay (tombstone edges) are not read for inline property bytes phase 1
([ADR 0016](../adr/0016-overflow-log-tombstones-and-src-fields.md), historical layout).

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

Dense-eligible buckets keep the **single-pass** parallel edge+inline property bytes read in
`visit_dense_out_edge_inline_property_batches_for_bucket` (bulk `read_slots_contiguous` +
inline property bytes). A phase 1+2 adapter was tried and reverted: full-batch callers
(vector expand, high match-rate filters) regressed on canbench versus one IO pass.

Hybrid and sparse buckets keep dedicated combined-batch paths.

## Facade layer (`GraphStore`)

**Crate:** `gleaph-graph` — `facade/store/edge_scan.rs`

| Method | Status | Role |
|--------|--------|------|
| `visit_out_edge_inline_property_batches_for_label` | Implemented | Combined batch; keep for simple callers |
| `visit_out_payload_value_batches_for_label` | Implemented (dense + hybrid + sparse) | Phase 1 only |
| `read_directed_out_edge_slots_for_label` | Implemented | Phase 2 only |
| `for_each_directed_out_edges_for_label_topology_unchecked` | Implemented | No inline property bytes |

Executor routing:

| Workload | Traversal pattern |
|----------|-------------------|
| Hop-count `ShortestPath` | Topology only (`load_payloads: false`) |
| Weighted `ShortestPath` | Bulk inline property bytes + bulk edge for **all** live slots (can use phase 1 + phase 2 with full slot list; no filter between phases) |
| `Expand` + inline property bytes predicate | Phase 1 → filter → phase 2 on **dense** and **overflow** hubs (M6c + hybrid replay cache) |
| `Expand` + equality index | Index → slot set → phase 2 (inline property bytes optional if index key is not inline property bytes-derived) |
| `Expand` + vector threshold | Combined batch + filter indices (inline-property-bytes-first deferred — canbench regression at all tested scan/match sizes) |

## Executor integration (planned)

### Predicate expand

Replace “combined batch + filter indices” with:

1. `visit_out_payload_value_batches_for_label`
2. `PreparedLabeledEdgeInlinePropertyBatchKernel::collect_matching_value_indices(values, …)`
3. `read_directed_out_edge_slots_for_label(&match_slots, …)` → `ExpandDst` + `EdgeBinding`

**Expected win:** proportional to `(1 - match_rate) * degree * E::BYTES` per hub, plus avoided `Edge` struct work on rejected slots.

### Weighted shortest

No change to *which* slots are read. Optional refactor:

1. Phase 1: bulk inline property bytes per expand
2. Decode weights with `PreparedWeightDecoder` into a scratch `Vec<WeightedCost>` (no `profile.prepare()` per edge)
3. Phase 2: bulk edge rows, zip with weights in relax

This is an **ordering / decode** optimization, not inline-property-bytes-first filtering.

### Equality index

When postings yield `(label_id, slot_index)` for `src` on **forward** expand:

1. Skip phase 1 (index value already matched)
2. Phase 2 directly: `read_out_edge_slots_for_label(&indexed_slots, …)` per label bucket

Avoids full degree scan. **Reverse / undirected** expand still uses full adjacency scan plus canonical handle matching because postings store forward owner slots.

## Dense eligibility (unchanged)

Two named predicates decide the dense paths; both read the descriptor's raw no-log byte and the edge
plane's per-bucket spill run instead of the retired i32 log head (ADR 0097):

```text
/* executor dense pre-check: bucket_dense_inline_property_batch_eligible */
!is_tiny_mode()
&& degree > 0
&& inline_property_byte_width > 0
&& inline_property_bytes_log_byte == 0xFF   /* NO_LOG_BYTE: no inline-property-bytes log */
&& edge_run().is_none()                     /* no per-bucket edge spill run */
&& inline_property_bytes_slab_slots == degree

/* phase-1 bulk value read: bucket_dense_slab_inline_property_bytes_readable */
is_inline_property_bytes_allocated()
&& inline_property_byte_width > 0
&& inline_property_bytes_log_len == 0
&& inline_property_bytes_slab_slots == degree
```

The no-log byte and zero length are validating-constructor invariants on slab rows (tree rows carry
the same byte in front of their depth marker), so a decoded slab descriptor already satisfies the
byte/length half. The predicates differ because phase 1 reads no edge rows, while the combined batch
also needs both planes slab-resident; that is why only the batch predicate tests `edge_run()`. The
shared/vertex overflow log is retired (ADR 0097): there is no `overflow_log_head` to test, and a
bucket's edge spill is its own `edge_run()`.

Vertices that fail this (e.g. converging-hub **src** with 48 edges whose values spill,
`inline_property_bytes_slab_slots > degree`) stay on sparse/combined path until
sparse inline-property-bytes-first is designed.

## Sparse buckets (run-backed)

**Not in milestone 1.**

Options for later:

- Walk values-run rows in lockstep with the edge run (same live ordinal), emitting value batches without full `Edge` materialization
- Fold-to-slab maintenance to increase dense eligibility on hot hubs (production:
  `GraphStore` edge inserts drain the LARA maintenance queue via
  `post_edge_insert_maintenance_budget`; delete paths drain fully).
  Aggressive vertex-edge-span compaction at batch boundaries is **planned** in
  [bulk-ingest-finalize.md](./bulk-ingest-finalize.md) (not implemented).
- Property / equality index to reduce visited slots without full inline property bytes scan

## Migration plan

| Step | Deliverable | Verification |
|------|-------------|--------------|
| M0 | Document + bench scopes (`labeled_visit_payload_value_batches`, `labeled_read_edge_slots`) | canbench pattern runs |
| M1 | Dense `visit_out_payload_value_batches_for_label` | **Implemented** — `values.rs` batch order + parity tests |
| M2 | `read_out_edge_slots_for_label` (dense bulk + sparse/run) | **Implemented** — slot/order parity + phase-1/2 integration test |
| M3 | Facade wrappers + predicate expand switched | **Implemented** — dense + overflow use phase 1+2; sparse keeps combined batch in executor |
| M4 | Equality-index expand uses phase 2 only | **Implemented** — forward (`PointingRight`); reverse/undirected keep full-scan fallback |
| M5 | Weighted shortest: prepared decoder on relax hot path | **Implemented** — `PreparedWeightDecoder::decode`; optional zip refactor deferred |
| M6 | Sparse inline-property-bytes-first | **Implemented (LARA)** — overflow `visit_out_inline_property_value_batches`; hybrid slab prefix reads edge rows for tombstone gating; cached inline property bytes log chains *(historical: retired-log mechanism)* |
| M6a | Executor probe removal | **Implemented** — dense-eligibility pre-check before phase 1 |
| M6b | Hybrid tombstone gating + chain cache | **Implemented** — slab prefix filters edge tombstones before emitting inline property bytes slots; `read_inline_property_bytes_log_chain_entry`; phase-2 overflow chain cache *(historical: retired-log mechanism)* |
| M6c | Executor overflow routing | **Implemented** — phase 1 reuses hybrid overflow replay (`log_table`, `slot→log_idx`, slab delete set) for phase 2 (avoids chain rebuild + stable re-read). Naive routing regressed +23% on `expand_payload_skewed_2k`; replay fix **−32%** vs baseline (2026-06-15 canbench) *(historical: retired-log mechanism)* |

**Backward compatibility:** combined `LabeledEdgeInlinePropertyBatch` API remains; dense buckets use single-pass bulk read. Phase 1+2 APIs are for selective slot reads (predicate/index expand). Sparse-only paths unchanged in the executor.

### M6c insight (historical, retired-log mechanism): why naive overflow routing regressed

Overflow predicate expand must scan **all** inline property bytes to filter (low match rate does not skip inline property bytes IO). Combined batch decodes each live edge once while attaching inline property bytes. Naive inline-property-bytes-first moved topology work to phase 2, but **phase 2 re-built the overflow log chain and re-read stable memory per match slot**, duplicating work. Caching phase-1 replay (`log_table`, `slot_to_log_idx`, slab delete set) makes phase 2 decode only the **m** matching edges from the cached table. The hybrid slab prefix still reads edge rows when needed to keep tombstoned slab payloads out of phase 1.

## Benchmark expectations

| Bench | Expectation |
|-------|-------------|
| `expand_payload_skewed_{200a,2k}` | Default `canbench`: single-label hub + `edge_inline_property_predicate`. M6c replay: **−26%** / **−32%** vs pre-M6c baseline (2026-06-15). |
| `large_expand_payload_skewed_{10k,50k}` | `canbench_large`: heavier graph construction; inline-property-bytes-first vs combined neutral to ~2–4% faster at 50k scan. |
| `expand_skewed_noise_*` | Two-label topology filter (not inline property bytes predicate); unchanged by inline-property-bytes-first routing. |
| `weighted_shortest_edge_cost_cache` | **No** gain from inline-property-bytes-first alone; gain from prepared decoder + dense src path |
| `hop_count_shortest_converging_hub` | Unchanged (topology-only) |
| New: `labeled_visit_payload_value_batches` | Isolates phase-1 IO |

## Open questions

1. **Tombstones in dense inline property bytes-only scan:** Resolved for dense-eligible buckets — `stored_slots == degree` excludes in-slab tombstones; phase 1 reads inline property bytes only. Phase 2 skips deleted slots on invariant drift.
2. **Reverse / in-edges:** Resolved — incoming inline-property-bytes-first expand mirrors the outgoing contract.
   `read_in_edge_slots_for_label_with_replay` (LARA bidirectional) and
   `GraphStore::read_in_edge_slots_for_label_reusing_payload_scratch` reuse the reverse phase-1
   hybrid replay; the executor (`PointingLeft` predicate expand) carries the same `value_scratch`
   into phase 2. Benchmark: `payload_first_incoming_log_backed_selective_match`.
3. **Undirected expand:** May require two directed phase-2 reads or a dedicated undirected slot resolver.
4. **ADR:** Not required for M1–M3 (API addition + executor routing). Consider ADR if sparse inline-property-bytes-first changes the sparse values-run scan contract.

## Source of truth

| Layer | Path |
|-------|------|
| LARA traversal | `crates/ic-stable-lara/src/labeled/graph/traverse.rs` |
| Batch types | `crates/ic-stable-lara/src/labeled/graph/iter.rs` |
| GraphStore edge scan | `crates/graph/src/facade/store/edge_scan.rs` |
| Expand routing | `crates/graph/src/plan/query/executor/expand/candidates.rs` |
| Shortest expand | `crates/graph/src/plan/query/executor/path.rs` |
| Inline property bytes decode | `crates/graph/src/edge_inline_property_scalar_codec.rs` |

## Related

- [labeled-edge-inline-properties.md](./labeled-edge-inline-properties.md) — storage layout and invariants
- [lara-and-facade.md](./lara-and-facade.md) — GraphStore vs LARA boundaries
- [index/property-index.md](../index/property-index.md) — equality index postings
- [execution/operators.md](../execution/operators.md) — `Expand`, `ShortestPath`
