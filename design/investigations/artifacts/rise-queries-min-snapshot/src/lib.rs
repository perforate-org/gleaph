//! Scratch extraction of RISE's query layer (upstream `AngeloSav/rise-rs` @ 3b178df,
//! MIT, © Angelo Savino & Rossano Venturini) for the Gleaph slice-7 feasibility spike.
//!
//! Extraction boundary (see inventory.md next to this crate):
//! - kept: the two index traits (`posting_iter`), TopKHeap, block posting metadata
//!   (in-memory accessors only), the scorer contract with BM25/DotScorer, and the WAND /
//!   Block-Max WAND / BM-MaxScore operators;
//! - stripped: clap CLI enums + epserde type-hash peek helpers (`peek_scorer_kind`),
//!   epserde persistence (derive macros, serialize/deserialize, load_file/create_file),
//!   log/indicatif progress plumbing, mem_dbg supertrait on `InvertedIndex`, the
//!   block-partitioning builders and the boolean/plain-MaxScore operators;
//! - contract change: `TopKHeap` ordering now breaks score ties by docid ascending —
//!   upstream ordered by `f32::total_cmp` alone, leaving tie order unspecified.
//!
//! No nightly features; no external dependencies.

pub mod posting_iter;

pub mod queries;

#[cfg(test)]
mod stub_index;
