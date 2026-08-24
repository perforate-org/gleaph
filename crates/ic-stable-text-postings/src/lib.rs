//! Stable-memory text postings (physical layer).
//!
//! This crate owns **physical facts only**:
//!
//! - posting-list encodings and their cursor mechanics;
//! - block-store layout headers;
//! - merge cursors over sorted postings; and
//! - deterministic benchmark fixtures ([`corpus`]).
//!
//! **Boundary.** This crate must never know analyzers, scoring formulas, GQL, Router, or
//! graph semantics: documents arrive as plain term ids, and every structure here is judged
//! by storage facts alone. Analyzer identity and ranking policy belong to the text-index
//! layers above this crate. The DAAT / block-max-skipping top-k driver ([`topk`]) is the
//! query executor mechanism composing the existing kernels; it computes no scores itself —
//! scoring weights, per-posting score parts, and block-max values arrive verbatim from
//! callers.

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]
#![warn(missing_docs)]

pub mod blockmax;
pub mod corpus;
pub mod enc;
#[cfg(any(test, feature = "canbench"))]
mod expanded;
pub mod merge;
pub mod topk;

#[cfg(feature = "canbench")]
mod bench;

pub use corpus::{Corpus, CorpusConfig};
pub use enc::PostingReader;
pub use merge::{MergeCursor, MergeState};
