# ic-stable-bitset canbench

This directory contains a small `canbench` harness for `ic_stable_bitset::BitSet`.

## Benchmarks

- `bench_bitset_insert_1024`
- `bench_bitset_contains_1024`
- `bench_bitset_contains_65536_4096`
- `bench_bitset_truncate_2048_to_1024`
- `bench_bitset_reopen_after_journal_4096`
- `bench_bitset_truncate_large_suffix_65536_to_32768`
- `bench_bitset_checkpoint_after_full_journal_4096`
- `bench_bitset_reopen_after_large_snapshot_65536`
- `bench_bitset_remove_1024_head`
- `bench_bitset_remove_1024_mid`
- `bench_bitset_remove_1024_tail`
- `bench_bitset_remove_65536_head`
- `bench_bitset_remove_65536_mid`
- `bench_bitset_remove_65536_tail`

## Run

From this directory:

```bash
canbench --persist --show-summary
```

The harness builds a wasm canister and exercises the stable bitset through
`DefaultMemoryImpl`, matching the standard `ic_stable_structures` setup used by
other stable-memory crates.
