# ic-stable-roaring canbench

This directory contains a small `canbench` harness for `ic_stable_roaring::StableRoaringBitMap`.

## Benchmarks

- `bench_roaring_insert_1024`
- `bench_roaring_contains_1024`
- `bench_roaring_contains_65536_4096`
- `bench_roaring_truncate_2048_to_1024`
- `bench_roaring_reopen_after_journal_4096`
- `bench_roaring_reopen_after_pure_journal_4096`
- `bench_roaring_reopen_after_segmented_journal_4096`
- `bench_roaring_truncate_large_suffix_65536_to_32768`
- `bench_roaring_checkpoint_after_full_journal_4096`
- `bench_roaring_reopen_after_large_snapshot_65536`

## Run

From this directory:

```bash
canbench --persist --show-summary
```

The harness builds a wasm canister and exercises the stable roaring bitmap
through `DefaultMemoryImpl`, matching the standard `ic_stable_structures`
setup used by other stable-memory crates.

The replay-oriented reopen benches are synthetic workloads for future `init()`
optimization work:

- `bench_roaring_reopen_after_pure_journal_4096`
  - long `set`/`clear` runs with no checkpoint boundaries
- `bench_roaring_reopen_after_segmented_journal_4096`
  - mixed `set`/`clear`/`ensure_len`/`truncate` runs split into phases to stress
    journal replay
