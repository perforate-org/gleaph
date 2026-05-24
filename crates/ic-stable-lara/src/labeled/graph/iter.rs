//! Labeled outgoing-edge iterators.

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
    },
    lara::edge::{AscOutEdgesIter, OutEdgeSlabIter, OutEdgesIter},
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;
use std::{iter::FusedIterator, num::NonZero};

use super::{LabeledLaraGraph, OutEdgeOrder};

/// Reusable buffers for labeled edge-value batch traversal.
#[derive(Clone, Debug)]
pub struct LabeledEdgeValueBatchScratch<E> {
    /// Edge rows in the same order as the parallel value byte chunks.
    pub edges: Vec<E>,
    /// Flattened value bytes: `edges.len() * batch.width_code.byte_width()` bytes.
    pub value_bytes: Vec<u8>,
}

impl<E> Default for LabeledEdgeValueBatchScratch<E> {
    fn default() -> Self {
        Self {
            edges: Vec::new(),
            value_bytes: Vec::new(),
        }
    }
}

impl<E> LabeledEdgeValueBatchScratch<E> {
    /// Clears both reusable buffers while preserving allocation capacity.
    pub fn clear(&mut self) {
        self.edges.clear();
        self.value_bytes.clear();
    }
}

/// One batch of edges and their parallel value bytes for a single label bucket.
pub struct LabeledEdgeValueBatch<'a, E> {
    /// Label bucket visited by this batch.
    pub label_id: BucketLabelKey,
    /// Physical byte width of each edge value in this batch.
    pub width_code: crate::labeled::slot_index::ValueWidthCode,
    /// Scan order used for both `edges` and `value_bytes`.
    pub order: OutEdgeOrder,
    /// Edge rows in scan order.
    pub edges: &'a [E],
    /// Flattened edge-value bytes in the same order as `edges`.
    pub value_bytes: &'a [u8],
    /// `true` when the batch was read from contiguous resident edge/value slab spans.
    pub dense: bool,
}

/// Streaming iterator over outgoing edges in a fixed scan order (see
/// [`LabeledLaraGraph::desc_out_edges_iter`], [`LabeledLaraGraph::asc_out_edges_iter`], and
/// [`LabeledLaraGraph::out_edges_by_directedness_iter`]).
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

pub enum LabeledSpanIter<'a, E: CsrEdge, M: Memory> {
    Empty,
    Desc {
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        vertex: LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        label_id: BucketLabelKey,
        log_chains: Option<(Vec<u32>, Vec<u32>)>,
        iter: OutEdgesIter<'a, E, M>,
    },
    Asc {
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        vertex: LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        label_id: BucketLabelKey,
        log_chains: Option<(Vec<u32>, Vec<u32>)>,
        iter: AscOutEdgesIter<'a, E, M>,
    },
}

impl<'a, E, M> Iterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.kind {
                LabeledOutEdgesIterKind::Empty => return None,
                LabeledOutEdgesIterKind::BypassDesc { label_id, iter } => {
                    return iter.next().map(|edge| edge.with_label_id(label_id.raw()));
                }
                LabeledOutEdgesIterKind::BypassAsc { label_id, iter } => {
                    return iter.next().map(|edge| edge.with_label_id(label_id.raw()));
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
                    let bucket_index = base_bucket_index.checked_add(local as u32)?;
                    *current = self
                        .graph
                        .labeled_bucket_span_iter(
                            self.src,
                            self.order,
                            vertex,
                            buckets,
                            local,
                            bucket_index,
                        )
                        .ok()?;
                }
            }
        }
    }

    fn advance_by(&mut self, mut n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        loop {
            match &mut self.kind {
                LabeledOutEdgesIterKind::Empty => {
                    return Err(NonZero::new(n).expect("n > 0"));
                }
                LabeledOutEdgesIterKind::BypassDesc { iter, .. } => return iter.advance_by(n),
                LabeledOutEdgesIterKind::BypassAsc { iter, .. } => return iter.advance_by(n),
                LabeledOutEdgesIterKind::Buckets { current, .. } => match current {
                    LabeledSpanIter::Empty => match self.roll_to_next_bucket_span() {
                        Ok(()) => continue,
                        Err(()) => return Err(NonZero::new(n).expect("n > 0")),
                    },
                    LabeledSpanIter::Desc { iter, .. } => match iter.advance_by(n) {
                        Ok(()) => return Ok(()),
                        Err(left) => {
                            n = left.get();
                            *current = LabeledSpanIter::Empty;
                        }
                    },
                    LabeledSpanIter::Asc { iter, .. } => match iter.advance_by(n) {
                        Ok(()) => return Ok(()),
                        Err(left) => {
                            n = left.get();
                            *current = LabeledSpanIter::Empty;
                        }
                    },
                },
            }
        }
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
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

    /// Advances past the exhausted current span to the next non-empty bucket span, mirroring
    /// [`Iterator::next`] roll logic for [`LabeledOutEdgesIterKind::Buckets`].
    fn roll_to_next_bucket_span(&mut self) -> Result<(), ()> {
        let LabeledOutEdgesIterKind::Buckets {
            vertex,
            buckets,
            base_bucket_index,
            next_bucket,
            current,
        } = &mut self.kind
        else {
            return Err(());
        };
        loop {
            let local = match next_bucket.take() {
                Some(l) => l,
                None => {
                    *current = LabeledSpanIter::Empty;
                    return Err(());
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
            let bucket_index = match base_bucket_index.checked_add(local as u32) {
                Some(b) => b,
                None => continue,
            };
            *current = self
                .graph
                .labeled_bucket_span_iter(
                    self.src,
                    self.order,
                    vertex,
                    buckets,
                    local,
                    bucket_index,
                )
                .map_err(|_| ())?;
            return Ok(());
        }
    }
}

impl<E, M> Iterator for LabeledSpanIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::Desc {
                graph,
                src,
                vertex,
                bucket_index,
                bucket,
                label_id,
                log_chains,
                iter,
            } => iter.next().map(|edge| {
                let slot = edge.edge_slot_index_raw();
                graph.attach_edge_value(
                    *src,
                    vertex,
                    *bucket_index,
                    *bucket,
                    slot,
                    edge.with_label_id(label_id.raw()),
                    log_chains.as_ref(),
                )
            }),
            Self::Asc {
                graph,
                src,
                vertex,
                bucket_index,
                bucket,
                label_id,
                log_chains,
                iter,
            } => iter.next().map(|edge| {
                let slot = edge.edge_slot_index_raw();
                graph.attach_edge_value(
                    *src,
                    vertex,
                    *bucket_index,
                    *bucket,
                    slot,
                    edge.with_label_id(label_id.raw()),
                    log_chains.as_ref(),
                )
            }),
        }
    }

    fn advance_by(&mut self, n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        match self {
            Self::Empty => Err(NonZero::new(n).expect("n > 0")),
            Self::Desc { iter, .. } => iter.advance_by(n),
            Self::Asc { iter, .. } => iter.advance_by(n),
        }
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
        self.next()
    }
}

impl<E, M> FusedIterator for LabeledSpanIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}
