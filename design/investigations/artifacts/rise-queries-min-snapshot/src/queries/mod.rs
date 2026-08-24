//! Upstream `queries/mod.rs` trimmed for the extraction: clap `ValueEnum` selections
//! (`ScorerKind`, `QueryKind`) and the epserde+xxhash `peek_scorer_kind` CLI helper are
//! dropped; the operator traits and re-exports remain.

mod block_posting_metadata;
pub use block_posting_metadata::BlockPostingMetadata;

pub mod scorers;
pub use scorers::DocScorer;
pub use scorers::BM25;
pub use scorers::DotScorer;

pub mod topk_heap;

pub mod query_algorithms;
pub use query_algorithms::*;

pub trait QueryOperator {
    fn query_name() -> &'static str;

    fn query<I>(&mut self, idx: &I, terms: &[usize]) -> usize
    where
        I: InvertedIndex;

    fn retrieved_docs(&self) -> Vec<usize> {
        todo!()
    }
}

pub trait RankedQueryOperator: QueryOperator {
    fn topk(&self) -> &topk_heap::TopKHeap;
}

use crate::posting_iter::InvertedIndex;
