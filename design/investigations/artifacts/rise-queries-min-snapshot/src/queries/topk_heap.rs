use std::{
    cmp::{Ordering, Reverse},
    collections::BinaryHeap,
};

/// Implements a min heap with a limited capacity of `k` elements.
pub struct TopKHeap {
    heap: BinaryHeap<Reverse<PostingInfo>>,
    threshold: f32,
    k: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PostingInfo {
    pub docid: u64,
    pub frequency: f32,
}

impl Eq for PostingInfo {}

impl Ord for PostingInfo {
    // EXTRACTION CONTRACT CHANGE (Gleaph slice 7): upstream ordered by
    // `frequency.total_cmp` alone, leaving equal-score order unspecified (heap-internal).
    // We break ties by docid DESCENDING inside this ordering so that the min-heap's root
    // — and `into_sorted_vec()`'s tail semantics below — evict/emit equal-score hits in
    // docid-ascending preference: worse = lower score, then higher docid.
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.frequency
            .total_cmp(&other.frequency)
            .then_with(|| other.docid.cmp(&self.docid))
    }
}

impl PartialOrd for PostingInfo {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl TopKHeap {
    // returns docids of retrieved elements, ordered by descending score
    // NOTE: this implementation may be inefficient as it clones the whole heap before iterating over it
    pub fn into_sorted_vec(&self) -> Vec<PostingInfo> {
        self.heap
            .clone()
            .into_sorted_vec()
            .into_iter()
            .map(|x| x.0)
            .collect()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[inline]
    pub fn can_enter(&self, v: f32) -> bool {
        self.heap.len() < self.k || self.threshold < v
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    #[inline]
    pub fn new(k: usize) -> Self {
        TopKHeap {
            heap: BinaryHeap::with_capacity(k),
            threshold: 0.0,
            k,
        }
    }

    pub fn clear(&mut self) {
        self.heap.clear();
        self.threshold = 0.0;
    }

    #[inline]
    pub fn push(&mut self, score: f32) -> bool {
        self.push_with_id(0, score)
    }

    #[inline]
    pub fn push_with_id(&mut self, id: u64, score: f32) -> bool {
        if self.heap.len() < self.k {
            self.heap.push(Reverse(PostingInfo {
                docid: id,
                frequency: score,
            }));
            self.threshold = self.heap.peek().unwrap().0.frequency;
            return true;
        } else if score > self.threshold {
            self.heap.pop();
            self.heap.push(Reverse(PostingInfo {
                docid: id,
                frequency: score,
            }));
            self.threshold = self.heap.peek().unwrap().0.frequency;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic replacement for upstream's `gen_positive_sequence` (which pulled in
    /// `rand` via `crate::gen_sequences`): a fixed xorshift64 stream, same style as the
    /// Gleaph fixtures.
    fn deterministic_scores(n: usize, scale: u64) -> Vec<f32> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            let mut x = state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            state = x;
            (x % scale) as f32 / 1000.0
        };
        (0..n).map(|_| next()).collect()
    }

    #[test]
    fn test_topk_heap() {
        let mut heap = TopKHeap::new(3);
        heap.push(1.0);
        heap.push(2.0);
        assert_eq!(heap.len(), 2);
        heap.push(3.0);
        heap.push(4.0);
        assert_eq!(heap.threshold, 2.0);
        assert_eq!(heap.len(), 3);

        println!("{:?}", heap.heap);
        heap.push(5.0);
        assert!(heap.can_enter(5.0));
        assert_eq!(heap.threshold, 3.0);

        assert!(!heap.can_enter(0.5));
        heap.push(0.5);

        heap.push_with_id(7, 100.2);
        heap.push_with_id(9, 4.1);

        let docs: Vec<u64> = heap.into_sorted_vec().into_iter().map(|x| x.docid).collect();
        assert_eq!(docs, vec![7, 0, 9], "scores 100.2 / 5.0 / 4.1");

        heap.clear();
        assert_eq!(heap.len(), 0);
        assert_eq!(heap.threshold, 0.0);
    }

    /// EXTRACTION CONTRACT CHANGE check: equal scores coexist and emit in docid
    /// ascending order (upstream left this unspecified), and a full heap evicts the
    /// highest-docid entry among score-tied candidates.
    #[test]
    fn tied_scores_order_and_evict_by_docid() {
        let mut heap = TopKHeap::new(4);
        heap.push_with_id(9, 4.1);
        heap.push_with_id(8, 4.1);
        heap.push_with_id(2, 7.0);
        heap.push_with_id(5, 7.0);
        let docs: Vec<u64> = heap.into_sorted_vec().into_iter().map(|x| x.docid).collect();
        assert_eq!(docs, vec![2, 5, 8, 9], "(score desc, docid asc)");

        // Strictly-better candidate arrives: the victim must be 4.1@docid 9, not the
        // equally scored 4.1@docid 8.
        assert!(heap.can_enter(4.2));
        assert!(heap.push_with_id(1, 4.2));
        let docs: Vec<u64> = heap.into_sorted_vec().into_iter().map(|x| x.docid).collect();
        assert_eq!(docs, vec![2, 5, 1, 8]);
    }

    #[test]
    fn test_random_topk_heap() {
        let mut heap = TopKHeap::new(10);
        let v = deterministic_scores(1000, 10_000);

        for &x in &v {
            heap.push(x);
        }

        let mut sorted_v = v.clone();
        sorted_v.sort_by(|a, b| a.total_cmp(b));
        let check = sorted_v.iter().rev().take(10).copied().collect::<Vec<_>>();

        let in_heap = heap
            .into_sorted_vec()
            .into_iter()
            .map(|x| x.frequency)
            .collect::<Vec<_>>();

        assert_eq!(in_heap, check);
    }
}
