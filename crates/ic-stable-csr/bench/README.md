# `ic-stable-csr-canbench`

Wasm + PocketIC benchmarks for `ic_stable_csr::dgap::DgapEdgeStore::remove_slab_edge_at_local_index_physically`, `ic_stable_csr::csr::CsrGraphWithGcQueue::delete_vertex`, and the `segment_maintenance_decision` score gate. Stable backing uses [`DefaultMemoryImpl`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/struct.DefaultMemoryImpl.html) and [`MemoryManager`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/memory_manager/struct.MemoryManager.html).

## Build

From the repository root (default `target/` layout matches `canbench.yml`):

```bash
cargo build --release --target wasm32-unknown-unknown -p ic-stable-csr-canbench
```

## Run (`canbench`)

Install the CLI (`cargo install canbench`) and point `--runtime-path` at a [PocketIC](https://github.com/dfinity/pocketic) binary (or set `POCKET_IC_BIN`). Run **from this directory** so `canbench.yml` paths resolve.

```bash
cd crates/ic-stable-csr-canbench
canbench --runtime-path "${POCKET_IC_BIN:-$HOME/.local/bin/pocket-ic}" --show-summary bench_remove_slab
```

To measure the Phase D `delete_vertex` path, run:

```bash
cd crates/ic-stable-csr-canbench
canbench --runtime-path "${POCKET_IC_BIN:-$HOME/.local/bin/pocket-ic}" --show-summary bench_delete_vertex
```

If `canbench` reports a runtime digest mismatch, either let it re-download the expected PocketIC build or pass `--no-runtime-integrity-check` (see the repo’s canbench-runner skill).

Optional baseline update (writes `canbench_results.yml`):

```bash
canbench --runtime-path … --no-runtime-integrity-check --persist --show-summary bench_remove_slab
canbench --runtime-path … --no-runtime-integrity-check --persist --show-summary bench_delete_vertex
canbench --runtime-path … --no-runtime-integrity-check --persist --show-summary bench_segment_maintain
```

## Scenarios

| Benchmark | Setup |
|-----------|--------|
| `bench_remove_slab_physically_chain_32` | 32-vertex chain `0→1→…`; measures one physical remove at `(vid=0, local_index=0)`. |
| `bench_remove_slab_physically_chain_1024` | Same pattern at 1024 vertices so the `0..n` base scan does ~1023 slot-map updates when `remove_pos == 0`. |
| `bench_remove_slab_physically_tail_vertex_chain_1024` | Same chain; remove at `(vid=n-2, local_index=0)` (last edge in the chain) so `remove_pos` is large and the base-decrement suffix is usually empty. |
| `bench_delete_vertex_hub_star_1024` | Hub-and-spoke graph with 1024 vertices and bidirectional center spokes; deleting vertex `0` exercises the partial forward/reverse PMA resync path. |
| `bench_segment_maintain_small_noop` | Small leaf with one tombstone stays below the score gate and should return `Noop`. |
| `bench_segment_maintain_small_enqueue` | Small leaf crosses the soft tombstone ratio gate and should return `Enqueue`. |
| `bench_segment_maintain_large_enqueue_by_score` | Same tombstone ratio as the small case, but a larger span pushes the score over the enqueue threshold. |
| `bench_segment_maintain_strict_inline` | Tombstone ratio above the strict gate should return `InlineNow`. |
| `bench_segment_maintain_queue_pressure_inline` | Soft garbage plus queue pressure should return `InlineNow`. |
| `bench_segment_maintain_rebalance_window_enqueue` | A rebalance window hint should enqueue even with zero tombstones. |

Scopes inside the hot path (feature `canbench-rs` on `ic-stable-csr`):

- `remove_slab`: `dgap_remove_slab_slide`, `dgap_remove_slab_base_decrement`, `dgap_remove_slab_sync_pma_full`, `dgap_remove_slab_maintain`, `dgap_remove_slab_refresh_tail`
- `delete_vertex`: `dgap_delete_vertex_collect_touched`, `dgap_delete_vertex_out_neighbors`, `dgap_delete_vertex_in_neighbors`, `dgap_delete_vertex_refresh_tail`, `dgap_delete_vertex_sync_pma_forward`, `dgap_delete_vertex_sync_pma_reverse`, `dgap_delete_vertex_push_queue`
- `segment_maintain`: `dgap_segment_maintain_tombstone_ratio`, `dgap_segment_maintain_tombstone_score`, `dgap_segment_maintain_soft_garbage`, `dgap_segment_maintain_strict_garbage`, `dgap_segment_maintain_queue_pressure`

**Implementation note:** `remove_slab` uses chunked `read_edge_slab_span` / `write_edge_slab_span` for the slide, then a vertex-column pass that finds the first row with `base > remove_pos` (binary search when dense bases are non-decreasing), reads only the candidate PMA-dirty prefix window, decrements bases only on the suffix, then `sync_pma_edge_counts_for_segments` (or full sync). The `dgap_remove_slab_base_decrement` scope includes that scan (plus `O(log n)` binary search); `dgap_remove_slab_sync_pma_full` covers only the SEC write path.

**Implementation note:** `delete_vertex` now does a conservative partial PMA resync. It collects the live touched vertex set per column, compresses each set into contiguous vertex ranges, and then calls `sync_pma_meta_for_vertex_range` on forward and reverse separately before enqueuing GC work.

## Phase C: how to read results (go / no-go)

Compare scope instruction counts (see `canbench_results.yml` for a committed baseline):

1. **`dgap_remove_slab_base_decrement` vs `dgap_remove_slab_sync_pma_full`** — If the base scan is a **large fraction** of the full PMA resync, narrowing or indexing the `base > remove_pos` walk is more attractive.
2. **`dgap_remove_slab_slide` vs the rest** — Sliding the tail of the slab after a remove near the front is often **O(occupied tail)**; if it dominates, Phase C should address slide cost before micro-optimizing the base loop alone.
3. **Small `n` sanity** — On the 32-vertex chain, `sync_pma_full` can still cost millions of instructions because the work is tied to the **formatted PMA tree**, not only local degree; interpret large graphs in light of that.

**Recorded baseline (`canbench_results.yml`, PocketIC 10.0.0, after candidate-window base scan + partial SEC write):**

- **Chain 1024 (head remove):** total ~28.4M instructions; `slide` ~0.54M; `sync_pma_full` scope ~21.2M (SEC write only); `base_decrement` scope ~3.47M (candidate-window scan + `BTreeSet`). **PMA SEC write** still dominates.
- **Chain 1024 (tail remove, `vid=n-2`):** total ~5.14M; `slide` ~0.4K; `sync_pma_full` scope ~1.94M; `base_decrement` scope ~37.6K — the candidate-only prefix window pays off most when `remove_pos` is large.
- **Chain 32:** total ~18.5M; `sync_pma_full` scope ~18.1M; `base_decrement` ~120.4K; slide ~9.6K. This is slightly higher than the prior baseline, but still tiny versus SEC sync.

Re-run after code changes; refresh the YAML with `canbench --persist` when you want regression tracking.

The committed baseline in `canbench_results.yml` now contains both the `remove_slab` and `delete_vertex` scenarios.
It also includes the `segment_maintain` microbench cases used to compare ratio- and score-based gating across small and large leaves.
