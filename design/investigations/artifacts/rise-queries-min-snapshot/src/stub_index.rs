//! Trivially stubbed posting source standing in for RISE's compressed indexes, plus the
//! slice-7 correctness gate: WAND / Block-Max WAND / BM-MaxScore must reproduce brute
//! force exactly (including the new docid tie-break) on a tiny deterministic corpus.

use std::collections::HashMap;

use crate::posting_iter::{InvertedIndex, PostingListIter};
use crate::queries::query_algorithms::{BMMaxScore, BMWand, Wand};
use crate::queries::{
    BlockPostingMetadata, DocScorer, QueryOperator, RankedQueryOperator, BM25, DotScorer,
};
use crate::queries::topk_heap::PostingInfo;

/// Vec-backed inverted index: `lists[t]` holds ascending `(docid, freq)` pairs.
struct StubIndex {
    n_docs: usize,
    lists: Vec<Vec<(u64, u64)>>,
}

impl InvertedIndex for StubIndex {
    type IterType<'a> = StubIter<'a>;

    fn n_docs(&self) -> usize {
        self.n_docs
    }

    fn n_terms(&self) -> usize {
        self.lists.len()
    }

    fn get_plist_iter(&self, i: usize) -> StubIter<'_> {
        StubIter {
            list: &self.lists[i],
            pos: 0,
            n_docs: self.n_docs,
        }
    }
}

/// Cursor mirroring the upstream exhaustion convention: past-the-end reports
/// `current_doc() == n_docs` (the universe size), matching how the EF iterators behave.
struct StubIter<'a> {
    list: &'a [(u64, u64)],
    pos: usize,
    n_docs: usize,
}

impl<'a> PostingListIter for StubIter<'a> {
    fn current_doc(&self) -> u64 {
        self.list
            .get(self.pos)
            .map(|p| p.0)
            .unwrap_or(self.n_docs as u64)
    }

    fn current_pos(&self) -> usize {
        self.pos
    }

    fn next_geq(&mut self, lower_bound: u64) {
        while self.pos < self.list.len() && self.list[self.pos].0 < lower_bound {
            self.pos += 1;
        }
    }

    fn next_doc(&mut self) {
        if self.pos < self.list.len() {
            self.pos += 1;
        }
    }

    fn freq(&mut self) -> u64 {
        self.list[self.pos].1
    }

    fn len(&self) -> usize {
        self.list.len()
    }
}

/// 6 docs, 3 terms, block size 2 metadata. Docids/scores chosen so two docs tie exactly
/// under [`DotScorer`] (docs 0 and 4 both score 5) to exercise the tie-break contract.
fn stub_index() -> StubIndex {
    StubIndex {
        n_docs: 6,
        lists: vec![
            vec![(0, 2), (1, 1), (2, 3), (3, 1), (4, 2), (5, 1)],
            vec![(1, 1), (3, 4), (5, 2)],
            vec![(0, 1), (2, 1), (4, 1), (5, 5)],
        ],
    }
}

/// Block-size-2 static-partitioning metadata for the fixture above (weights =
/// `Scorer::doc_term_weight(freq, norm)` without query scaling, exactly like upstream's
/// builders produce).
fn dot_metadata() -> BlockPostingMetadata<DotScorer> {
    let norms_len: Box<[f32]> = vec![1.2, 0.8, 1.6, 0.4, 0.8, 1.2].into_boxed_slice();
    let max_term_weight: Box<[f32]> = vec![3.0, 4.0, 5.0].into_boxed_slice();
    // Per-term block starts into the flat arrays: 3 + 2 + 2 blocks.
    let blocks_start: Box<[usize]> = vec![0, 3, 5, 7].into_boxed_slice();
    let blocks_docid: Box<[u32]> = vec![1, 3, 5, 3, 5, 2, 5].into_boxed_slice();
    let blocks_max_term_weight: Box<[f32]> =
        vec![2.0, 3.0, 2.0, 4.0, 2.0, 1.0, 5.0].into_boxed_slice();
    BlockPostingMetadata::from_parts(
        norms_len,
        max_term_weight,
        blocks_start,
        blocks_docid,
        blocks_max_term_weight,
    )
}

/// Brute-force top-k oracle: score every document, order by (score desc, docid asc).
/// Used with [`DotScorer`] whose integer-valued sums are order-exact in f32.
fn brute_force(idx: &StubIndex, terms: &[usize], k: usize) -> Vec<PostingInfo> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for t in terms {
        *counts.entry(*t).or_insert(0) += 1;
    }
    let norms: Vec<f32> = vec![1.2, 0.8, 1.6, 0.4, 0.8, 1.2];
    let mut scores = vec![0.0f32; idx.n_docs];
    for (term, q_freq) in &counts {
        let q_weight = DotScorer::query_term_weight(
            *q_freq as u64,
            idx.lists[*term].len() as u64,
            idx.n_docs as u64,
        );
        for (docid, freq) in &idx.lists[*term] {
            scores[*docid as usize] +=
                q_weight * DotScorer::doc_term_weight(*freq, norms[*docid as usize]);
        }
    }
    let mut hits: Vec<PostingInfo> = scores
        .iter()
        .enumerate()
        .filter(|(_, score)| **score > 0.0)
        .map(|(docid, score)| PostingInfo {
            docid: docid as u64,
            frequency: *score,
        })
        .collect();
    hits.sort_by(|a, b| {
        b.frequency
            .total_cmp(&a.frequency)
            .then(a.docid.cmp(&b.docid))
    });
    hits.truncate(k);
    hits
}

const QUERY: &[usize] = &[0, 0, 1, 2];

#[test]
fn wand_matches_brute_force_with_exact_tie_break() {
    let idx = stub_index();
    let md = dot_metadata();
    let truth = brute_force(&idx, QUERY, 10);
    assert_eq!(truth.len(), 6);
    // Docs 0 and 4 tie at score 5: the expected order pins docid ascending.
    assert_eq!(truth[3].docid, 0);
    assert_eq!(truth[4].docid, 4);

    let mut op = Wand::new(&md, 10);
    assert_eq!(op.query(&idx, QUERY), 6);
    assert_eq!(op.topk().into_sorted_vec(), truth);
}

#[test]
fn bm_wand_matches_brute_force_with_exact_tie_break() {
    let idx = stub_index();
    let md = dot_metadata();
    let truth = brute_force(&idx, QUERY, 10);

    let mut op = BMWand::new(&md, 10);
    assert_eq!(op.query(&idx, QUERY), 6);
    assert_eq!(op.topk().into_sorted_vec(), truth);
}

#[test]
fn bm_maxscore_matches_brute_force_with_exact_tie_break() {
    let idx = stub_index();
    let md = dot_metadata();
    let truth = brute_force(&idx, QUERY, 10);

    let mut op = BMMaxScore::new(&md, 10);
    assert_eq!(op.query(&idx, QUERY), 6);
    assert_eq!(op.topk().into_sorted_vec(), truth);
}

#[test]
fn bm25_operators_run_and_agree_on_the_top_hit() {
    let idx = StubIndex {
        n_docs: 6,
        lists: vec![
            vec![(0, 2), (1, 1), (2, 3), (3, 1), (4, 2), (5, 1)],
            vec![(1, 1), (3, 4), (5, 2)],
            vec![(0, 1), (2, 1), (4, 1), (5, 5)],
        ],
    };
    // Same layout as dot_metadata, but weights are the exact BM25
    // `doc_term_weight(freq, norm)` values for this corpus (f / (f + 0.6 + 0.6*norm)),
    // so every stored block maximum truly bounds its block as the builders guarantee.
    let norms_len: Box<[f32]> = vec![1.2, 0.8, 1.6, 0.4, 0.8, 1.2].into_boxed_slice();
    let max_term_weight: Box<[f32]> =
        vec![0.657_894_74, 0.862_068_97, 0.791_139_24].into_boxed_slice();
    let blocks_start: Box<[usize]> = vec![0, 3, 5, 7].into_boxed_slice();
    let blocks_docid: Box<[u32]> = vec![1, 3, 5, 3, 5, 2, 5].into_boxed_slice();
    let blocks_max_term_weight: Box<[f32]> = vec![
        0.602_409_64, // t0 b{0,1}
        0.657_894_74, // t0 b{2,3}
        0.649_350_65, // t0 b{4,5}
        0.826_446_28, // t1 b{1,3}
        0.862_068_97, // t1 b{5}
        0.431_034_48, // t2 b{0,2}
        0.791_139_24, // t2 b{4,5}
    ]
    .into_boxed_slice();
    let md = BlockPostingMetadata::from_parts(
        norms_len,
        max_term_weight,
        blocks_start,
        blocks_docid,
        blocks_max_term_weight,
    );

    // Smoke gate: every operator executes and returns all six scored docs. Exact BM25
    // ordering is not asserted against brute force here because upstream's
    // `query_freqs` builds its term vector from a HashMap (iteration order varies per
    // process), so equal-score accumulation order is unspecified upstream — recorded as
    // a determinism finding for Task D.
    let top_from_wand = {
        let mut op = Wand::<BM25>::new(&md, 10);
        assert_eq!(op.query(&idx, QUERY), 6);
        op.topk().into_sorted_vec()
    };
    let mut op = BMWand::<BM25>::new(&md, 10);
    assert_eq!(op.query(&idx, QUERY), 6);
    assert_eq!(op.topk().into_sorted_vec()[0].docid, top_from_wand[0].docid);
    let mut op = BMMaxScore::<BM25>::new(&md, 10);
    assert_eq!(op.query(&idx, QUERY), 6);
}
