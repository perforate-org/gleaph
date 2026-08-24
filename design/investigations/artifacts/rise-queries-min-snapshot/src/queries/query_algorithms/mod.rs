//! Upstream `queries/query_algorithms/mod.rs` trimmed to the three operators this
//! extraction compiles (WAND, Block-Max WAND, BM-MaxScore); the boolean and plain
//! MaxScore operators are excluded from the minimal set. `query_freqs` is verbatim.

use std::collections::HashMap;

pub use bm_maxscore::BMMaxScore;
pub use bm_wand::BMWand;
pub use wand::Wand;

mod bm_maxscore;
mod bm_wand;
mod wand;

#[inline]
// given a vector of terms, returns a vector of pairs (term, frequency in query)
fn query_freqs(terms: &[usize]) -> Vec<(usize, usize)> {
    let mut count: HashMap<usize, usize> = HashMap::new();

    for term in terms {
        *count.entry(*term).or_insert(0) += 1;
    }

    count.into_iter().collect::<Vec<_>>()
}
