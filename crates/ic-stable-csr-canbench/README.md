# `ic-stable-csr-canbench`

Wasm + PocketIC benchmarks for `ic_stable_csr::dgap::DgapEdgeStore::remove_slab_edge_at_local_index_physically`. Stable backing uses [`DefaultMemoryImpl`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/struct.DefaultMemoryImpl.html) and [`MemoryManager`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/memory_manager/struct.MemoryManager.html) (three virtual memories: `M_v`, PMA `segment_edge_counts`, edges+log).

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

If `canbench` reports a runtime digest mismatch, either let it re-download the expected PocketIC build or pass `--no-runtime-integrity-check` (see the repo’s canbench-runner skill).

Optional baseline update (writes `canbench_results.yml`):

```bash
canbench --runtime-path … --no-runtime-integrity-check --persist --show-summary bench_remove_slab
```

## Scenarios

| Benchmark | Setup |
|-----------|--------|
| `bench_remove_slab_physically_chain_32` | 32-vertex chain `0→1→…`; measures one physical remove at `(vid=0, local_index=0)`. |
| `bench_remove_slab_physically_chain_1024` | Same pattern at 1024 vertices so the `0..n` base scan does ~1023 slot-map updates when `remove_pos == 0`. |

Scopes inside the hot path (feature `canbench-rs` on `ic-stable-csr`): `dgap_remove_slab_slide`, `dgap_remove_slab_base_decrement`, `dgap_remove_slab_sync_pma_full`, `dgap_remove_slab_maintain`, `dgap_remove_slab_refresh_tail`.

**Implementation note:** `remove_slab` uses chunked `read_edge_slab_span` / `write_edge_slab_span` for the slide and dirty-segment `sync_pma_edge_counts_for_segments` after the remove (scope label `dgap_remove_slab_sync_pma_full` is historical).

## Phase C: how to read results (go / no-go)

Compare scope instruction counts (see `canbench_results.yml` for a committed baseline):

1. **`dgap_remove_slab_base_decrement` vs `dgap_remove_slab_sync_pma_full`** — If the base scan is a **large fraction** of the full PMA resync, narrowing or indexing the `base > remove_pos` walk is more attractive.
2. **`dgap_remove_slab_slide` vs the rest** — Sliding the tail of the slab after a remove near the front is often **O(occupied tail)**; if it dominates, Phase C should address slide cost before micro-optimizing the base loop alone.
3. **Small `n` sanity** — On the 32-vertex chain, `sync_pma_full` can still cost millions of instructions because the work is tied to the **formatted PMA tree**, not only local degree; interpret large graphs in light of that.

**Recorded baseline (`canbench_results.yml`, PocketIC 0.4.x, after bulk slide + partial PMA sync):**

- **Chain 1024:** total ~30.5M instructions; `slide` ~0.54M; `sync_pma_full` scope ~23.6M; `base_decrement` ~3.1M. Slide is no longer the bottleneck on this scenario; **PMA resync** dominates the remainder.
- **Chain 32:** total ~18.6M; `sync_pma_full` scope ~18.1M; slide ~9.6K — **formatted PMA** still dominates at small `n`.

Re-run after code changes; refresh the YAML with `canbench --persist` when you want regression tracking.
