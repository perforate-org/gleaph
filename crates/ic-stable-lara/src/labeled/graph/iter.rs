//! Labeled outgoing-edge iterators.

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
    },
    lara::{
        edge::{AscOutEdgesIter, OutEdgeSlabIter, OutEdgesIter},
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;
use std::{iter::FusedIterator, num::NonZero};

use super::{LabeledLaraGraph, LabeledOperationError, OutEdgeOrder};

/// Reusable buffers for labeled edge-payload batch traversal.
#[derive(Clone, Debug)]
pub struct LabeledEdgePayloadBatchScratch<E> {
    /// Edge rows in the same order as the parallel value byte chunks.
    pub edges: Vec<E>,
    /// Flattened value bytes: `edges.len() * batch.byte_width.byte_width()` bytes.
    pub payload_bytes: Vec<u8>,
}

impl<E> Default for LabeledEdgePayloadBatchScratch<E> {
    fn default() -> Self {
        Self {
            edges: Vec::new(),
            payload_bytes: Vec::new(),
        }
    }
}

impl<E> LabeledEdgePayloadBatchScratch<E> {
    /// Clears both reusable buffers while preserving allocation capacity.
    pub fn clear(&mut self) {
        self.edges.clear();
        self.payload_bytes.clear();
    }
}

/// One batch of edges and their parallel value bytes for a single label bucket.
pub struct LabeledEdgePayloadBatch<'a, E> {
    /// Label bucket visited by this batch.
    pub label_id: BucketLabelKey,
    /// Physical byte width of each edge payload in this batch.
    pub byte_width: u16,
    /// Scan order used for both `edges` and `payload_bytes`.
    pub order: OutEdgeOrder,
    /// Edge rows in scan order.
    pub edges: &'a [E],
    /// Flattened edge-payload bytes in the same order as `edges`.
    pub payload_bytes: &'a [u8],
    /// `true` when the batch was read from contiguous resident edge/payload slab spans.
    pub dense: bool,
}

/// Streaming iterator over outgoing edges in a fixed scan order.
///
/// Items are fallible because labeled edge rows may need to read payload bytes from stable
/// payload storage. Use `collect::<Result<Vec<_>, _>>()` when materializing rows. For paging or
/// OFFSET-style traversal, prefer [`LabeledOutEdgesIter::try_advance_by`] over
/// [`Iterator::nth`] or [`Iterator::skip`] so skipped rows do not read payload bytes.
pub struct LabeledOutEdgesIter<'a, E: CsrEdge, M: Memory> {
    pub(super) graph: &'a LabeledLaraGraph<E, M>,
    pub(super) src: VertexId,
    pub(super) order: OutEdgeOrder,
    pub(super) kind: LabeledOutEdgesIterKind<'a, E, M>,
}

pub(super) enum LabeledOutEdgesIterKind<'a, E: CsrEdge, M: Memory> {
    Empty,
    BypassDesc {
        label_id: BucketLabelKey,
        iter: OutEdgeSlabIter<'a, E, M>,
    },
    BypassAsc {
        label_id: BucketLabelKey,
        iter: AscOutEdgesIter<'a, E, M>,
    },
    Buckets {
        vertex: LabeledVertex,
        buckets: Vec<LabelBucket>,
        base_bucket_index: u32,
        next_bucket: Option<usize>,
        current: LabeledSpanIter<'a, E, M>,
    },
}

#[doc(hidden)]
pub struct LabeledBucketScan<'a, E: CsrEdge, M: Memory> {
    graph: &'a LabeledLaraGraph<E, M>,
    src: VertexId,
    vertex: LabeledVertex,
    bucket_index: u32,
    bucket: LabelBucket,
    label_id: BucketLabelKey,
    log_chains: Option<(Vec<u32>, Vec<u32>)>,
    kind: LabeledBucketScanKind<'a, E, M>,
}

pub(super) enum LabeledBucketScanKind<'a, E: CsrEdge, M: Memory> {
    Desc { iter: OutEdgesIter<'a, E, M> },
    Asc { iter: AscOutEdgesIter<'a, E, M> },
}

pub enum LabeledSpanIter<'a, E: CsrEdge, M: Memory> {
    Empty,
    Scan(LabeledBucketScan<'a, E, M>),
}

impl<'a, E, M> Iterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = Result<E, LabeledOperationError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.kind {
                LabeledOutEdgesIterKind::Empty => return None,
                LabeledOutEdgesIterKind::BypassDesc { label_id, iter } => {
                    return iter
                        .next()
                        .map(|edge| Ok(edge.with_label_id(label_id.raw())));
                }
                LabeledOutEdgesIterKind::BypassAsc { label_id, iter } => {
                    return iter
                        .next()
                        .map(|edge| Ok(edge.with_label_id(label_id.raw())));
                }
                LabeledOutEdgesIterKind::Buckets {
                    vertex,
                    buckets,
                    base_bucket_index,
                    next_bucket,
                    current,
                } => {
                    if let Some(edge) = current.next() {
                        return Some(edge);
                    }
                    let local = next_bucket.take()?;
                    if self.order == OutEdgeOrder::Descending {
                        *next_bucket = local.checked_sub(1);
                    } else {
                        let next = local + 1;
                        if next < buckets.len() {
                            *next_bucket = Some(next);
                        }
                    }
                    if buckets[local].degree() == 0 {
                        continue;
                    }
                    let Some(bucket_index) = base_bucket_index.checked_add(local as u32) else {
                        self.kind = LabeledOutEdgesIterKind::Empty;
                        return Some(Err(LaraOperationError::CollectAllocationOverflow.into()));
                    };
                    match self.graph.labeled_bucket_span_iter(
                        self.src,
                        self.order,
                        vertex,
                        buckets,
                        local,
                        bucket_index,
                    ) {
                        Ok(span) => *current = span,
                        Err(err) => {
                            self.kind = LabeledOutEdgesIterKind::Empty;
                            return Some(Err(err));
                        }
                    }
                }
            }
        }
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        for _ in 0..n {
            match self.next()? {
                Ok(_) => {}
                Err(err) => return Some(Err(err)),
            }
        }
        self.next()
    }
}

impl<'a, E, M> FusedIterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

impl<'a, E, M> LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn empty(
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Self {
        Self {
            graph,
            src,
            order,
            kind: LabeledOutEdgesIterKind::Empty,
        }
    }

    /// Advances by live rows without reading skipped payload bytes.
    ///
    /// Returns `Ok(Err(left))` when the iterator ends before all `n` rows are skipped, and
    /// returns `Err` for storage/layout failures encountered while moving between bucket spans.
    pub fn try_advance_by(
        &mut self,
        mut n: usize,
    ) -> Result<Result<(), NonZero<usize>>, LabeledOperationError> {
        if n == 0 {
            return Ok(Ok(()));
        }
        loop {
            match &mut self.kind {
                LabeledOutEdgesIterKind::Empty => {
                    return Ok(Err(NonZero::new(n).expect("n > 0")));
                }
                LabeledOutEdgesIterKind::BypassDesc { iter, .. } => {
                    return Ok(iter.advance_by(n));
                }
                LabeledOutEdgesIterKind::BypassAsc { iter, .. } => {
                    return Ok(iter.advance_by(n));
                }
                LabeledOutEdgesIterKind::Buckets { current, .. } => match current.try_advance_by(n)
                {
                    Ok(()) => return Ok(Ok(())),
                    Err(left) => {
                        n = left.get();
                        *current = LabeledSpanIter::Empty;
                        if self.roll_to_next_bucket_span()? {
                            continue;
                        }
                        return Ok(Err(NonZero::new(n).expect("n > 0")));
                    }
                },
            }
        }
    }

    /// Advances past the exhausted current span to the next non-empty bucket span.
    fn roll_to_next_bucket_span(&mut self) -> Result<bool, LabeledOperationError> {
        let LabeledOutEdgesIterKind::Buckets {
            vertex,
            buckets,
            base_bucket_index,
            next_bucket,
            current,
        } = &mut self.kind
        else {
            return Ok(false);
        };
        loop {
            let local = match next_bucket.take() {
                Some(l) => l,
                None => {
                    *current = LabeledSpanIter::Empty;
                    return Ok(false);
                }
            };
            if self.order == OutEdgeOrder::Descending {
                *next_bucket = local.checked_sub(1);
            } else {
                let next = local + 1;
                if next < buckets.len() {
                    *next_bucket = Some(next);
                }
            }
            if buckets[local].degree() == 0 {
                continue;
            }
            let bucket_index = base_bucket_index
                .checked_add(local as u32)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            *current = self.graph.labeled_bucket_span_iter(
                self.src,
                self.order,
                vertex,
                buckets,
                local,
                bucket_index,
            )?;
            return Ok(true);
        }
    }
}

impl<E, M> Iterator for LabeledBucketScan<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = Result<E, LabeledOperationError>;

    fn next(&mut self) -> Option<Self::Item> {
        let edge = match &mut self.kind {
            LabeledBucketScanKind::Desc { iter } => iter.next()?,
            LabeledBucketScanKind::Asc { iter } => iter.next()?,
        };
        let slot = edge.edge_slot_index_raw();
        Some(self.graph.attach_edge_payload(
            self.src,
            &self.vertex,
            self.bucket_index,
            self.bucket,
            slot,
            edge.with_label_id(self.label_id.raw()),
            self.log_chains.as_ref(),
        ))
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        for _ in 0..n {
            match self.next()? {
                Ok(_) => {}
                Err(err) => return Some(Err(err)),
            }
        }
        self.next()
    }
}

impl<E, M> Iterator for LabeledSpanIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = Result<E, LabeledOperationError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::Scan(scan) => scan.next(),
        }
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        for _ in 0..n {
            match self.next()? {
                Ok(_) => {}
                Err(err) => return Some(Err(err)),
            }
        }
        self.next()
    }
}

impl<E, M> LabeledBucketScan<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub fn try_advance_by(&mut self, n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        match &mut self.kind {
            LabeledBucketScanKind::Desc { iter } => iter.advance_by(n),
            LabeledBucketScanKind::Asc { iter } => iter.advance_by(n),
        }
    }
}

impl<E, M> LabeledSpanIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn desc<'a>(
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        vertex: LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        label_id: BucketLabelKey,
        log_chains: Option<(Vec<u32>, Vec<u32>)>,
        iter: OutEdgesIter<'a, E, M>,
    ) -> LabeledSpanIter<'a, E, M> {
        LabeledSpanIter::Scan(LabeledBucketScan {
            graph,
            src,
            vertex,
            bucket_index,
            bucket,
            label_id,
            log_chains,
            kind: LabeledBucketScanKind::Desc { iter },
        })
    }

    pub(super) fn asc<'a>(
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        vertex: LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        label_id: BucketLabelKey,
        log_chains: Option<(Vec<u32>, Vec<u32>)>,
        iter: AscOutEdgesIter<'a, E, M>,
    ) -> LabeledSpanIter<'a, E, M> {
        LabeledSpanIter::Scan(LabeledBucketScan {
            graph,
            src,
            vertex,
            bucket_index,
            bucket,
            label_id,
            log_chains,
            kind: LabeledBucketScanKind::Asc { iter },
        })
    }

    pub fn try_advance_by(&mut self, n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        match self {
            Self::Empty => Err(NonZero::new(n).expect("n > 0")),
            Self::Scan(scan) => scan.try_advance_by(n),
        }
    }
}

impl<E, M> FusedIterator for LabeledSpanIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}
