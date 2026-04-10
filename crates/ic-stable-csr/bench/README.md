# `ic-stable-csr-canbench`

`canbench` benchmark harness for `ic-stable-csr`, focused on comparing deleted-vertex strategies:

- `RowTombstone`
- `SparseDeleted` (`ic-stable-roaring`)
- `DenseDeleted` (`ic-stable-bitset`)

The benchmark set is organized to answer three questions:

1. Which variant is cheaper for a given operation?
2. Which graph / delete pattern favors each variant?
3. Should service default stay `SparseDeleted`, or switch to `DenseDeleted`?

`RowTombstone` is included as a low-level raw-traversal comparison target. It is not a service-default candidate because it intentionally does not provide logical iterators.

## Build

From the repository root:

```bash
cargo build --release --target wasm32-unknown-unknown -p ic-stable-csr-canbench
```

## Run

Install [`canbench`](https://github.com/dfinity/canbench) and point it at a PocketIC binary. Run from this directory so `canbench.yml` resolves correctly.

```bash
cd /Users/yota/dev/gleaph-project/crates/ic-stable-csr/bench
canbench --runtime-path "${POCKET_IC_BIN:-$HOME/.local/bin/pocket-ic}" --show-summary bench_delete_vertex
```

If PocketIC digest validation gets in the way during local iteration, use `--no-runtime-integrity-check`.

Persist a refreshed baseline:

```bash
canbench --runtime-path "${POCKET_IC_BIN:-$HOME/.local/bin/pocket-ic}" --no-runtime-integrity-check --persist --show-summary bench_delete_vertex
```

## Benchmark Layers

### 1. Micro

Deleted-index-only microbenchmarks live in the dedicated crates:

- `crates/ic-stable-bitset/bench`
- `crates/ic-stable-roaring/bench`

Use those for:

- `insert`
- `contains`
- `reopen`
- truncate / remove / checkpoint behavior

This CSR harness intentionally focuses on graph-level costs. In particular, `reopen` differences between roaring and bitset should be judged from those dedicated index benches, because `ic-stable-csr` currently exposes `format_new` constructors rather than a graph-level reopen API.

### 2. Operation

Single-operation benchmarks compare direct graph costs across variants.

Operation benchmarks are fixture-based:

- build a graph once outside the measured closure
- snapshot stable memory
- restore + `open_existing_*` before the measured operation

This means `delete`, `gc`, and `read` instruction counts exclude graph construction time.

Implemented groups:

- `bench_build_fixture_*`
- `bench_delete_vertex_*`
- `bench_delete_edge_*`
- `bench_gc_step_*`
- `bench_raw_read_*`
- `bench_logical_read_*` (`SparseDeleted` / `DenseDeleted` only)

### 3. Scenario

Service-like mixed workloads:

- `bench_scenario_read_heavy_*`
- `bench_scenario_mixed_*`
- `bench_scenario_delete_heavy_*`

These are the main inputs for service-default decisions.

## Topologies, Delete Patterns, Densities

The harness has generators for:

- topologies: `chain`, `hub_star`, `uniform_random_sparse`, `power_law`, `clustered_community`
- delete patterns: `uniform_random`, `clustered_contiguous`, `hub_first`, `leaf_first`
- delete densities: `0.1%`, `1%`, `10%`, `50%`

Committed baseline scenarios stay intentionally small and reviewable. Large `256k` graphs are for manual local runs.

## Naming

Bench names follow:

```text
bench_<operation>_<variant>_<graph>_<scale>_<delete-pattern>_<density>
```

Examples:

- `bench_delete_vertex_dense_hub_star_1024_hub_first_d10pct`
- `bench_gc_step_sparse_uniform_random_sparse_32768_uniform_random_d1pct`
- `bench_raw_read_row_clustered_community_32768_clustered_contiguous_d10pct`

## Implemented Comparison Matrix

### Build Fixture

- `bench_build_fixture_row_uniform_random_sparse_8192`
- `bench_build_fixture_sparse_uniform_random_sparse_8192`
- `bench_build_fixture_dense_uniform_random_sparse_8192`

### Delete Vertex

- `bench_delete_vertex_row_hub_star_1024_hub_first_d10pct`
- `bench_delete_vertex_sparse_hub_star_1024_hub_first_d10pct`
- `bench_delete_vertex_dense_hub_star_1024_hub_first_d10pct`
- `bench_delete_vertex_sparse_uniform_random_sparse_32768_uniform_random_d1pct`
- `bench_delete_vertex_dense_uniform_random_sparse_32768_uniform_random_d1pct`

### Delete Edge

- `bench_delete_edge_row_uniform_random_sparse_32768_uniform_random_d1pct`
- `bench_delete_edge_sparse_uniform_random_sparse_32768_uniform_random_d1pct`
- `bench_delete_edge_dense_uniform_random_sparse_32768_uniform_random_d1pct`
- `bench_delete_edge_row_clustered_community_32768_clustered_contiguous_d10pct`
- `bench_delete_edge_sparse_clustered_community_32768_clustered_contiguous_d10pct`
- `bench_delete_edge_dense_clustered_community_32768_clustered_contiguous_d10pct`

### GC Step

- `bench_gc_step_row_hub_star_1024_hub_first_d10pct`
- `bench_gc_step_sparse_hub_star_1024_hub_first_d10pct`
- `bench_gc_step_dense_hub_star_1024_hub_first_d10pct`
- `bench_gc_step_row_clustered_community_32768_clustered_contiguous_d10pct`
- `bench_gc_step_sparse_clustered_community_32768_clustered_contiguous_d10pct`
- `bench_gc_step_dense_clustered_community_32768_clustered_contiguous_d10pct`

### Read

- raw:
  - `bench_raw_read_row_uniform_random_sparse_32768_uniform_random_d1pct`
  - `bench_raw_read_sparse_uniform_random_sparse_32768_uniform_random_d1pct`
  - `bench_raw_read_dense_uniform_random_sparse_32768_uniform_random_d1pct`
- logical:
  - `bench_logical_read_sparse_uniform_random_sparse_32768_uniform_random_d1pct`
  - `bench_logical_read_dense_uniform_random_sparse_32768_uniform_random_d1pct`

### Scenarios

- `bench_scenario_read_heavy_sparse_uniform_random_sparse_32768_uniform_random_d1pct`
- `bench_scenario_read_heavy_dense_uniform_random_sparse_32768_uniform_random_d1pct`
- `bench_scenario_mixed_sparse_power_law_32768_uniform_random_d10pct`
- `bench_scenario_mixed_dense_power_law_32768_uniform_random_d10pct`
- `bench_scenario_delete_heavy_sparse_power_law_32768_uniform_random_d10pct`
- `bench_scenario_delete_heavy_dense_power_law_32768_uniform_random_d10pct`

Legacy DGAP/PMA maintenance benchmarks remain available:

- `bench_remove_slab_physically_*`
- `bench_segment_maintain_*`

## Scopes

The harness records these scope names:

- raw read:
  - `dgap_raw_read_scan`
- logical read:
  - `dgap_logical_read_scan`
  - `dgap_logical_read_deleted_filter`
  - `dgap_logical_read_yield`
- gc:
  - `dgap_gc_step_run_leaf`
  - `dgap_gc_step_pop_queue`

Existing DGAP scopes from the remove-slab and segment-maintenance benches are still emitted as before.

## How To Read Results

### Delete Vertex / Delete Edge

Compare total instructions first, then heap / stable-memory increase.

Ignore any historical results that included graph construction inside the measured closure. The fixture-based benches are the source of truth for operation cost comparisons.

- If `SparseDeleted` and `DenseDeleted` are close, prefer `SparseDeleted` unless deletion density stays high in service traffic.
- If `DenseDeleted` clearly wins on both hub-heavy and random topologies, that is a signal to investigate it as a default candidate.

### GC Step

Use GC runs to answer whether a strategy only shifts cost from delete time into maintenance time.

- If delete is cheap but `gc_step` balloons, the variant may not help end-to-end.
- Compare `queue_len_before`, `queue_len_after`, and `completed_gc_items` in the benchmark summaries.

### Raw vs Logical Read

- `RowTombstone` should only be judged on raw read.
- `SparseDeleted` vs `DenseDeleted` should be judged on logical read, because that is the service-facing traversal path.
- Pay attention to `dgap_logical_read_deleted_filter` when delete density rises.

### Scenario Runs

These decide the default.

Current decision rule:

- default hypothesis: `SparseDeleted`
- switch to `DenseDeleted` only if:
  - `DenseDeleted` wins both `delete_heavy` and `mixed`
  - `DenseDeleted` is at least neutral on `read_heavy`

`RowTombstone` is never the service default.

## Recommendation Table

| Variant | Recommended use |
|--------|------------------|
| `RowTombstone` | Low-level or internal use where logical traversal is unnecessary and minimum structural overhead matters most |
| `SparseDeleted` | Service default unless delete density is consistently high |
| `DenseDeleted` | Use when delete density remains high and logical traversal stays hot |

## Manual Large-Graph Runs

The committed baseline is intentionally smaller than the full matrix. For large local comparison runs, use explicit names and update only when you want to keep the result:

```bash
cd /Users/yota/dev/gleaph-project/crates/ic-stable-csr/bench
canbench --runtime-path "${POCKET_IC_BIN:-$HOME/.local/bin/pocket-ic}" --show-summary bench_scenario_mixed_dense_power_law_32768_uniform_random_d10pct
```

For `256k`-scale runs, keep them manual and ad hoc rather than committing every result to `canbench_results.yml`.

## Verification Notes

Development-time checks for this harness:

```bash
cargo check -p ic-stable-csr-canbench
cargo test -p ic-stable-csr-canbench
cargo test -p ic-stable-csr
```

`canbench_results.yml` should only be refreshed from actual `canbench --persist` runs. Do not hand-edit benchmark totals.
