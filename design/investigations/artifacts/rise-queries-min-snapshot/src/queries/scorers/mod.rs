mod bm25;
mod dot_scorer;

pub use bm25::BM25;
pub use dot_scorer::DotScorer;

/// Upstream scoring contract verbatim minus the `TypeHash` supertrait (epserde): that
/// bound existed only so serialised indexes could fingerprint their scorer. The operator
/// code uses only the two weight functions.
pub trait DocScorer {
    /// Term-frequency component of the document-side score.
    ///
    /// * `freq` — in-document term frequency.
    /// * `norm_len` — document length normalised by the average document length.
    fn doc_term_weight(freq: u64, norm_len: f32) -> f32;

    /// IDF-like query-side weight for a term.
    ///
    /// * `freq` — query-term frequency.
    /// * `df` — number of documents containing the term.
    /// * `num_docs` — total number of documents in the collection.
    fn query_term_weight(freq: u64, df: u64, num_docs: u64) -> f32;
}
