//! DAAT / block-max-skipping top-k query executor over existing posting kernels.
//!
//! Pruning structure reference-ported from RISE, MIT, github.com/AngeloSav/rise-rs
//! @ 3b178df: pivot evaluation over frontiers kept sorted by current docid, whole-total
//! block-max skipping, and worst-at-top bounded-heap admission — reimplemented here in
//! this crate's style (integer scores, cached frontiers, insertion-fixed ordering) rather
//! than copied. Promoted out of bench-only gating as the production query-executor
//! mechanism: the text index's search path drives these types directly over live
//! segments.
//!
//! Each [`QueryList`] pairs a reader (any [`PostingReader`] codec) with the score each of
//! its postings contributes — a constant caller-supplied weight plus a caller-built
//! tf→part lookup table ([`TfPartTable`], 256 entries; stored tfs are capped at 255) the
//! driver applies inline at every candidate — and weight-scaled per-block upper bounds.
//! **Boundary.** This module owns no scoring math and no analyzer: contributions are
//! summed verbatim, integers only, no floats; ranking formulas, tokenization, and the
//! part-table contents belong to layers above (plan 0295: positional score-part vectors
//! were replaced by this table so candidates are scored lazily from the codec's tf).
//! Per-candidate economics (plan 0296): each consumed posting costs exactly ONE fused
//! [`PostingReader::next_step`] dispatch plus a cached frontier read — the former
//! peek/freq/next triple crossed the reader boundary three times per candidate.
//!
//! Allocation discipline: the main loop performs zero allocations. Frontier/order caches
//! and the bounded heap are sized once up front; ordering is maintained by stable
//! insertion-fixes over cached `Option<u32>` keys (no per-iteration sort over trait-
//! dispatched peeks, no collects). The skip rule is the verified sound whole-total form:
//! when the sum of every active list's frontier-block maximum cannot reach the heap
//! threshold, no document before the earliest next-block start can qualify, so every
//! cursor advances there at once (a monotone frontier transform, which preserves the
//! sorted invariant for free).

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use crate::blockmax::LOGICAL_BLOCK_SIZE;
use crate::enc::PostingReader;

/// Caller-built tf→part lookup table: contribution part for a posting whose stored term
/// frequency is the index. Codecs without stored frequencies report tf 1, so constant-
/// weight callers pass an all-zero table. Indexes above 255 are unreachable (encoders
/// cap tfs at 255).
pub type TfPartTable = [u32; 256];

/// One disjunctive query list: a reader over the query postings (codec chosen by the
/// caller), a constant contribution added for every posting match, the caller-built
/// tf→part table applied inline at each candidate, and weight-scaled per-block upper
/// bounds (caller-supplied, aligned to [`LOGICAL_BLOCK_SIZE`]).
pub struct QueryList<'a, R> {
    reader: R,
    weight: u32,
    block_bounds: &'a [u32],
    tf_parts: &'a TfPartTable,
}

impl<'a, R: PostingReader> QueryList<'a, R> {
    /// Pairs `reader` with its caller-supplied constant `weight`, per-block upper
    /// bounds, and the caller-built tf→part table.
    pub fn new(reader: R, weight: u32, block_bounds: &'a [u32], tf_parts: &'a TfPartTable) -> Self {
        Self {
            reader,
            weight,
            block_bounds,
            tf_parts,
        }
    }

    /// Cached-frontier accessor: next unconsumed docid without consuming it.
    fn frontier(&mut self) -> Option<u32> {
        self.reader.peek()
    }

    /// Block-max upper bound covering `docid`.
    fn bound_at(&self, docid: u32) -> u32 {
        self.block_bounds[(docid / LOGICAL_BLOCK_SIZE) as usize]
    }

    /// Consumes the frontier posting through ONE fused codec step and returns its total
    /// contribution — the list's constant weight plus the caller's table entry at the
    /// posting's tf — together with the refreshed cached frontier. The refresh reads the
    /// codec's lazy cursor (no second decode), so a full traversal decodes each posting
    /// exactly once.
    fn take_step(&mut self) -> (u32, Option<u32>) {
        let (_, tf) = self.reader.next_step().expect("frontier live at call");
        let part = self.tf_parts[tf.min(255) as usize];
        (self.weight + part, self.reader.peek())
    }

    /// Advances to the first posting at or beyond the skip target.
    fn skip_to(&mut self, target: u32) {
        let _ = self.reader.advance(target);
    }
}

/// A scored document kept by the top-k heap; ordering prefers higher score, then lower
/// docid (deterministic tie-break).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Total integer contribution summed verbatim across matched lists.
    pub score: u32,
    /// The scored document id.
    pub docid: u32,
}

impl Ord for Hit {
    /// Natural "better-greater" order: higher scores rank greater, and among equal
    /// scores lower docids rank greater. Combined with [`Reverse`] in the driver's
    /// max-heap, the heap top is always the worst kept hit (lowest score, then highest
    /// docid), giving the deterministic (score desc, docid asc) result contract.
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.docid.cmp(&self.docid))
    }
}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// First docid of the logical block after `docid`.
fn next_block_start(docid: u32) -> u32 {
    (docid / LOGICAL_BLOCK_SIZE + 1) * LOGICAL_BLOCK_SIZE
}

/// Restores ascending order over cached frontiers with one stable insertion pass. Inputs
/// are nearly sorted after every mutation the driver performs, so real work is ~O(n);
/// the key is a cached `Option<u32>`, never a trait-dispatched peek. `None` (exhausted)
/// sorts first and is compacted away at the top of each iteration.
fn sort_by_cached_frontier(order: &mut [usize], frontiers: &mut [Option<u32>]) {
    debug_assert_eq!(order.len(), frontiers.len());
    for i in 1..frontiers.len() {
        let mut p = i;
        while p > 0 && frontiers[p - 1] > frontiers[p] {
            order.swap(p - 1, p);
            frontiers.swap(p - 1, p);
            p -= 1;
        }
    }
}

// Test-only probe counting whole-total skip rounds so the prune-path test can prove
// the skipping branch actually ran rather than merely passing by luck.
#[cfg(test)]
thread_local! {
    static SKIP_ROUNDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Disjunctive DAAT top-k with block-max pruning over `lists` (one per query term).
///
/// Public production entry point of the promoted driver. Results carry the
/// (score desc, docid asc) contract; every score consumed here was supplied by the
/// caller via [`QueryList`].
/// Per iteration: exhausted cursors are compacted away; when the heap is full and the
/// sum of every active list's frontier-block maximum cannot reach the required score,
/// all cursors skip to the earliest next-block start (whole-total soundness: each list
/// contributes at most its frontier-block maximum to any such doc, and nothing once a
/// doc precedes its frontier). Otherwise the smallest frontier docid is evaluated: the
/// contiguous run of cursors sitting on it contributes their postings' scores, and the
/// result competes against the heap's worst entry under the (score desc, docid asc)
/// tie-break.
pub fn topk_disjunctive<R: PostingReader>(lists: &mut [QueryList<'_, R>], k: usize) -> Vec<Hit> {
    debug_assert!(!lists.is_empty());
    debug_assert!(k >= 1);

    // Sized once; nothing below allocates. The heap is a max-heap over `Reverse<Hit>`,
    // so its top is always the worst kept hit under the (score desc, docid asc) contract.
    let mut heap: BinaryHeap<Reverse<Hit>> = BinaryHeap::with_capacity(k);
    let mut order: Vec<usize> = (0..lists.len()).collect();
    let mut frontiers: Vec<Option<u32>> = Vec::with_capacity(lists.len());
    for list in lists.iter_mut() {
        frontiers.push(list.frontier());
    }
    sort_by_cached_frontier(&mut order, &mut frontiers);

    while !order.is_empty() {
        // Compaction: drop exhausted cursors (they sort first, so a prefix sweep ends at
        // the first live entry).
        if frontiers[0].is_none() {
            let live = frontiers.partition_point(|f| f.is_none());
            order.drain(..live);
            frontiers.drain(..live);
            if order.is_empty() {
                break;
            }
        }
        debug_assert!(
            frontiers.windows(2).all(|w| w[0] <= w[1]),
            "frontiers must stay ascending"
        );

        // Threshold: when the heap is full, a candidate must beat its worst entry.
        if heap.len() == k {
            let required = heap.peek().expect("full").0.score + 1;
            let mut total_bound = 0u32;
            let mut can_qualify = false;
            for (r, &idx) in order.iter().enumerate() {
                let frontier = frontiers[r].expect("compacted");
                total_bound = total_bound.saturating_add(lists[idx].bound_at(frontier));
                if total_bound >= required {
                    can_qualify = true;
                    break;
                }
            }
            if !can_qualify {
                #[cfg(test)]
                SKIP_ROUNDS.with(|c| c.set(c.get() + 1));
                // Whole-total skip: no doc before the earliest next-block start can
                // qualify. Advancing every cursor there is a monotone frontier
                // transform for survivors; re-sorting over cached keys keeps the
                // invariant airtight when cursors run off their list's end (None sorts
                // first and the next iteration compacts them away).
                let jump = order
                    .iter()
                    .zip(frontiers.iter())
                    .map(|(_, f)| next_block_start(f.expect("compacted")))
                    .min()
                    .expect("non-empty");
                for (idx, f) in order.iter_mut().zip(frontiers.iter_mut()) {
                    lists[*idx].skip_to(jump);
                    *f = lists[*idx].frontier();
                }
                sort_by_cached_frontier(&mut order, &mut frontiers);
                continue;
            }
        }

        // Evaluate the smallest frontier docid: the contiguous prefix of cursors sitting
        // on it contributes their postings' scores. Each cursor is consumed through one
        // fused (docid, tf) codec step — no separate peek/freq/next dispatches.
        let candidate = frontiers[0].expect("live");
        let mut score = 0u32;
        let mut matched = 0usize;
        while matched < frontiers.len() && frontiers[matched] == Some(candidate) {
            let (contribution, next) = lists[order[matched]].take_step();
            score += contribution;
            frontiers[matched] = next;
            matched += 1;
        }
        // Consumed cursors moved strictly past `candidate`, but they may land on either
        // side of untouched suffix entries, so re-insertion-sort the whole cached pair
        // (nearly sorted: real work is the moved cursors only; keys are cached values,
        // never trait-dispatched peeks).
        sort_by_cached_frontier(&mut order, &mut frontiers);

        let rev = Reverse(Hit {
            score,
            docid: candidate,
        });
        if heap.len() < k {
            heap.push(rev);
        } else {
            let mut worst = heap.peek_mut().expect("full");
            // Replace iff the newcomer beats the incumbent worst: `rev < *worst` reads,
            // through Reverse, as "hit is better than the kept worst".
            if rev < *worst {
                *worst = rev;
            }
        }
    }

    // Result contract: (score desc, docid asc).
    let mut out: Vec<Hit> = heap.into_vec().into_iter().map(|rev| rev.0).collect();
    out.sort_by(|a, b| b.score.cmp(&a.score).then(a.docid.cmp(&b.docid)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::{FreqVarintReader, encode_freq_varint};

    /// Brute-force oracle over (docid, part) lists: sum parts at common docs, order by
    /// (score desc, docid asc), truncate to k.
    fn brute_force(lists: &[&[(u32, u32)]], k: usize) -> Vec<Hit> {
        let mut hits: Vec<Hit> = Vec::new();
        let mut cursors = vec![0usize; lists.len()];
        loop {
            let candidate = lists
                .iter()
                .zip(cursors.iter())
                .filter(|(list, cur)| **cur < list.len())
                .map(|(list, cur)| list[*cur].0)
                .min();
            let Some(candidate) = candidate else { break };
            let mut score = 0u32;
            for (list, cur) in lists.iter().zip(cursors.iter_mut()) {
                if *cur < list.len() && list[*cur].0 == candidate {
                    score += list[*cur].1;
                    *cur += 1;
                }
            }
            match hits.binary_search_by(|h| h.docid.cmp(&candidate)) {
                Ok(_) => unreachable!("candidates are distinct"),
                Err(pos) => hits.insert(
                    pos,
                    Hit {
                        score,
                        docid: candidate,
                    },
                ),
            }
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.docid.cmp(&b.docid)));
        hits.truncate(k);
        hits
    }

    /// Drives the operator over freq-varint-encoded docid streams. Each entry is a
    /// `(docid, tf)` posting list plus its constant weight; block bounds are derived
    /// honestly as the max contribution (weight + tf) per docid-block, and the shared
    /// identity part table maps every tf to itself (contribution = weight + tf).
    fn run(lists: &[(&[(u32, u32)], u32)], k: usize) -> Vec<Hit> {
        let encoded: Vec<Vec<u8>> = lists
            .iter()
            .map(|(postings, _)| {
                let docs: Vec<u32> = postings.iter().map(|p| p.0).collect();
                let tfs: Vec<u32> = postings.iter().map(|p| p.1).collect();
                encode_freq_varint(&docs, &tfs)
            })
            .collect();
        let n_blocks = lists
            .iter()
            .flat_map(|(postings, _)| postings.iter().map(|p| p.0))
            .max()
            .map(|max_docid| (max_docid / LOGICAL_BLOCK_SIZE + 1) as usize)
            .unwrap_or(1);
        let all_bounds: Vec<Vec<u32>> = lists
            .iter()
            .map(|(postings, weight)| {
                let mut bounds = vec![0u32; n_blocks];
                for &(docid, tf) in postings.iter() {
                    let entry = &mut bounds[(docid / LOGICAL_BLOCK_SIZE) as usize];
                    *entry = (*entry).max(weight + tf);
                }
                bounds
            })
            .collect();
        let identity_parts: Box<TfPartTable> = Box::new(std::array::from_fn(|tf| tf as u32));

        let mut qlists: Vec<QueryList<'_, FreqVarintReader<'_>>> = Vec::with_capacity(lists.len());
        for i in 0..lists.len() {
            let (_, weight) = lists[i];
            qlists.push(QueryList::new(
                FreqVarintReader::new(&encoded[i]),
                weight,
                &all_bounds[i],
                &identity_parts,
            ));
        }

        SKIP_ROUNDS.with(|c| c.set(0));
        topk_disjunctive(&mut qlists, k)
    }

    fn skip_rounds() -> usize {
        SKIP_ROUNDS.with(|c| c.get())
    }

    /// Worst-case prune-path gate: a dense high-scoring lead list fills the heap early,
    /// forcing repeated whole-total skips across the sparse tails' blocks, eviction
    /// churn once tail hits arrive, mid-query exhaustion of the shortest tail, and an
    /// exact three-list score tie broken by docid. Output must equal brute force
    /// throughout, and the skip branch must demonstrably have run.
    #[test]
    fn pruned_driver_matches_brute_force_on_adversarial_fixture() {
        // Dense lead: alternating strong/weak parts across five full blocks. Docs 300
        // and 301 are withheld so they can be re-composed into an exact tie below.
        let lead: Vec<(u32, u32)> = (0..640)
            .filter(|d| *d != 300 && *d != 301)
            .map(|d| (d, if d % 2 == 0 { 10 } else { 3 }))
            .collect();
        // Sparse tail crossing many blocks past the lead's end (forces long skip runs).
        let mut mid: Vec<(u32, u32)> = (0..=1280).step_by(97).map(|d| (d, 1 + d % 3)).collect();
        mid.push((300, 5));
        mid.push((301, 7));
        mid.sort_unstable(); // unique docids ⇒ orders pairs by docid
        // Shortest tail exhausts mid-query; its doc-300 entry completes the tie:
        // doc 300 = 5 (mid) + 2 (tail) = 7 = doc 301 (mid alone).
        let tail: Vec<(u32, u32)> = vec![(300, 2), (500, 4), (900, 4)];

        let lists: [(&[(u32, u32)], u32); 3] = [
            (lead.as_slice(), 0),
            (mid.as_slice(), 0),
            (tail.as_slice(), 0),
        ];
        let truth = brute_force(&lists.map(|(postings, _)| postings), 5);
        assert_eq!(truth.len(), 5);

        let first = run(&lists, 5);
        assert_eq!(first, truth, "pruned driver must match brute force");
        // Contract shape: score desc, then docid asc among equal scores — including
        // the crafted 7-point tie at docs 300/301.
        assert!(
            first.windows(2).all(|w| w[0].score > w[1].score
                || (w[0].score == w[1].score && w[0].docid < w[1].docid)),
            "output contract violated: {first:?}"
        );
        assert!(skip_rounds() > 0, "whole-total skip path must have run");

        // Deterministic replays, including a permuted list order (same set semantics).
        assert_eq!(run(&lists, 5), first);
        let permuted: [(&[(u32, u32)], u32); 3] = [
            (tail.as_slice(), 0),
            (lead.as_slice(), 0),
            (mid.as_slice(), 0),
        ];
        assert_eq!(run(&permuted, 5), first);
    }

    /// Constant-weight mode (empty parts) still works through the same driver, matching
    /// the plain-weight benches' semantics: contribution = weight per matched list.
    #[test]
    fn constant_weight_mode_matches_brute_force() {
        let a: Vec<(u32, u32)> = vec![(1, 0), (64, 0), (200, 0)];
        let b: Vec<(u32, u32)> = vec![(64, 0), (129, 0)];
        // Weights 3 and 2; parts stay zero so contributions are pure weights.
        let lists = [(a.as_slice(), 3u32), (b.as_slice(), 2u32)];
        let truth = {
            // doc 64 matches both: 5; docs 1, 200: 3; doc 129: 2
            let mut hits = vec![
                Hit {
                    score: 5,
                    docid: 64,
                },
                Hit { score: 3, docid: 1 },
                Hit {
                    score: 3,
                    docid: 200,
                },
                Hit {
                    score: 2,
                    docid: 129,
                },
            ];
            hits.sort_by(|x, y| y.score.cmp(&x.score).then(x.docid.cmp(&y.docid)));
            hits.truncate(3);
            hits
        };
        assert_eq!(run(&lists, 3), truth);
    }
}
