# RISE extraction inventory (slice 7, Task A)

Upstream: AngeloSav/rise-rs @ `3b178df` ("fix typo"), depth-1 clone. Edition 2024.
Six crate-level nightly features declared in `src/lib.rs`: `impl_trait_in_assoc_type`,
`iter_array_chunks`, `array_windows`, `core_intrinsics`, `float_algebraic`,
`binary_heap_into_iter_sorted`.

## External-dep occurrences in candidate sets

Legend: ● = direct `use` in file. Only files with at least one occurrence are listed.

| File | epserde | mem_dbg | clap | num | rand | indicatif | log | xxhash-rust |
|---|---|---|---|---|---|---|---|---|
| queries/mod.rs | ● (`TypeHash`, inside `peek_scorer_kind`) | | ● (`ValueEnum` derive) | | | | | ● (inside `peek_scorer_kind`) |
| queries/block_posting_metadata.rs | ● (`prelude`: derive + ser/de) | | | | | (via `utils::pb_with_message`) | ● (`info!`) | |
| queries/scorers/mod.rs | ● (`prelude`; `DocScorer: TypeHash` supertrait) | | | | | | | |
| queries/scorers/bm25.rs | ● (`Epserde` derive) | | | | | | | |
| queries/scorers/dot_scorer.rs | ● (`Epserde` derive) | | | | | | | |
| queries/block_partitioning.rs | | | | ● (`num::Float` — **vestigial**, bodies use inherent `f32::max` only) | | | | |
| queries/topk_heap.rs | | | | | (test-only, via `crate::gen_sequences` → `rand`) | | | |
| elias_fano/elias_fano.rs | ● | | | ● (`integer::div_ceil`) | | | | |
| elias_fano/{strict_ef,uniform_partitioned_seq,opt_partition,complement_ef,all_ones_seq,indexed_seq,indexed_seq_complement}.rs | ● (derives) | | | ● (`ranked_bv.rs` uses `integer::div_ceil`) | | | | |
| bitvector/mod.rs (+bitvector_collection.rs) | ● | ● | | | | | | |
| positive_sequences/positive_sequence.rs | ● | | | | | | | |

Not appearing anywhere in the candidate sets: rayon, memmap2, divan, generic-tests,
dsi-bitstream, rgb, rusty-perm, env_logger, num-traits. Where they really live:
rayon/memmap2 → `indexes/freq_index_builder.rs`, `readers/ds2i_reader.rs`, bins;
env_logger/divan/generic-tests → binaries/tests outside candidates; dsi-bitstream →
`indexes/.../interpolative_coding.rs`; rgb/rusty-perm → `bin/read_write_rgb.rs`;
epserde-mmap feature → index load paths only.

## Nightly-feature usage per construct (grep level)

| Feature | Real usage sites in candidates |
|---|---|
| `core_intrinsics` | `queries/query_algorithms/{and,or,ranked_and}.rs` via `core::intrinsics::likely` (3 files); also `elias_fano/{elias_fano,opt_partition}.rs`, `positive_sequences/positive_sequence.rs` — **not used by WAND/BM-WAND/BM-MaxScore** |
| `float_algebraic` | **commented-out code only** (`queries/scorers/bm25.rs:17–23`); active BM25 uses plain ops |
| `array_windows` | `elias_fano/all_ones_seq.rs:26` only (outside queries) |
| `impl_trait_in_assoc_type` | **declared but unused** — no `type X = impl Y` found; GATs (`type IterType<'a> = Concrete<'a>`) are stable since 1.65 |
| `iter_array_chunks` | **declared but unused** — no `.array_chunks()` found |
| `binary_heap_into_iter_sorted` | **declared but unused** — topk_heap uses stable `into_sorted_vec()` |

## LOC counts (total / after `#[cfg(test)]` marker)

| Set | total | test-only |
|---|---|---|
| queries/** | 1921 | 62 |
| — topk_heap.rs | 163 | 61 |
| — mod.rs | 90 | 0 |
| — block_posting_metadata.rs | 212 | 0 |
| — block_partitioning.rs | 79 | 0 |
| — score_part.rs | 213 | 0 |
| — scorers/{mod,bm25,dot_scorer} | 82 | 0 |
| — query_algorithms/{mod,wand,bm_wand,bm_maxscore} | 636 | 0 |
| — query_algorithms/{and,or,maxscore,ranked_and,ranked_or} | 439 | 0 |
| elias_fano/** | 3378 | 2 |
| bitvector/** | 2679 | 64 |
| positive_sequences/** | 215 | 2 |
| indexes/** (context only, not a candidate) | 3902 | 1195 |
| readers/** (context only) | 183 | 0 |
| config.rs / utils.rs / gen_sequences.rs / lib.rs | 30 / 238 / 78 / 37 | 0 |

## Key structural facts feeding Tasks B/C/D

1. The query operators depend on exactly two traits from `indexes/mod.rs`:
   `PostingListIter { current_doc, current_pos, next_geq, next_doc, freq, len }` and
   `InvertedIndex { type IterType<'a>, n_docs, n_terms, get_plist_iter }`.
2. `InvertedIndex: MemSize + MemDbg` (mem_dbg) is a **supertrait bound** — mem_dbg is
   load-bearing for the trait declaration unless the bound is dropped (it is unused by
   the operator bodies).
3. epserde reaches the operators through: `#[derive(Epserde)]` on
   BlockPostingMetadata/BM25/DotScorer, the `DocScorer: TypeHash` supertrait,
   serialize/deserialize in create_file/load_file, and type-hash peek helpers
   (`peek_scorer_kind`). All of these are persistence/CLI plumbing, not algorithmic.
4. Exhaustion sentinel convention: iterators report `current_doc() == n_docs`
   (universe size) past the end (see `NextGEQ` doc: returns `(u, len)`).
