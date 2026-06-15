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
    /// Reusable bulk-read buffer for contiguous edge slab IO.
    pub(super) io_edge_bytes: Vec<u8>,
    /// Reusable bulk-read buffer for contiguous payload slab IO.
    pub(super) io_payload_bytes: Vec<u8>,
}

impl<E> Default for LabeledEdgePayloadBatchScratch<E> {
    fn default() -> Self {
        Self {
            edges: Vec::new(),
            payload_bytes: Vec::new(),
            io_edge_bytes: Vec::new(),
            io_payload_bytes: Vec::new(),
        }
    }
}

impl<E> LabeledEdgePayloadBatchScratch<E> {
    /// Clears both reusable buffers while preserving allocation capacity.
    pub fn clear(&mut self) {
        self.edges.clear();
        self.payload_bytes.clear();
    }

    pub(super) fn io_edge_slice_mut(&mut self, len: usize) -> &mut [u8] {
        if self.io_edge_bytes.len() < len {
            self.io_edge_bytes.resize(len, 0);
        }
        &mut self.io_edge_bytes[..len]
    }

    pub(super) fn io_edge_slice(&self, len: usize) -> &[u8] {
        &self.io_edge_bytes[..len]
    }

    pub(super) fn io_payload_slice_mut(&mut self, len: usize) -> &mut [u8] {
        if self.io_payload_bytes.len() < len {
            self.io_payload_bytes.resize(len, 0);
        }
        &mut self.io_payload_bytes[..len]
    }

    pub(super) fn io_payload_slice(&self, len: usize) -> &[u8] {
        &self.io_payload_bytes[..len]
    }
}

/// Cached overflow-log replay from hybrid payload phase 1 for phase-2 topology reads.
///
/// When present, [`super::LabeledLaraGraph::read_out_edge_slots_for_label`] can decode
/// matching overflow-log edges from the cached segment table instead of rebuilding the
/// ascending log chain and re-reading stable memory.
#[derive(Clone, Debug, Default)]
pub struct HybridOverflowEdgeReplay {
    pub(super) leaf: u32,
    pub(super) label_id: BucketLabelKey,
    pub(super) slab_slots: u32,
    pub(super) deleted_slab_offsets: Vec<u32>,
    pub(super) log_table: Vec<u8>,
    pub(super) slot_to_log_idx: std::collections::BTreeMap<u32, u32>,
}

impl HybridOverflowEdgeReplay {
    pub fn is_active(&self) -> bool {
        self.slab_slots > 0 || !self.slot_to_log_idx.is_empty()
    }

    pub(super) fn clear(&mut self) {
        self.leaf = 0;
        self.label_id = BucketLabelKey::from_raw(0);
        self.slab_slots = 0;
        self.deleted_slab_offsets.clear();
        self.log_table.clear();
        self.slot_to_log_idx.clear();
    }
}

/// Reusable buffers for payload-only batch traversal (phase 1).
#[derive(Clone, Debug, Default)]
pub struct LabeledPayloadValueBatchScratch {
    /// Absolute edge slot index per value chunk in `values`.
    pub slot_indices: Vec<u32>,
    /// Flattened payload bytes: `slot_indices.len() * byte_width` when emitted.
    pub values: Vec<u8>,
    /// Populated by hybrid overflow payload phase 1 for phase-2 slot reads.
    pub hybrid_overflow_replay: HybridOverflowEdgeReplay,
}

impl LabeledPayloadValueBatchScratch {
    /// Clears batch buffers while preserving hybrid overflow replay for phase 2.
    pub fn clear(&mut self) {
        self.slot_indices.clear();
        self.values.clear();
    }

    /// Clears batch buffers and any cached hybrid overflow replay.
    pub fn clear_all(&mut self) {
        self.clear();
        self.hybrid_overflow_replay.clear();
    }
}

/// One batch of parallel payload bytes for a single label bucket (no edge rows).
pub struct LabeledPayloadValueBatch<'a> {
    /// Label bucket visited by this batch.
    pub label_id: BucketLabelKey,
    /// Physical byte width of each payload chunk in `values`.
    pub byte_width: u16,
    /// Scan order used for `slot_indices` and `values`.
    pub order: OutEdgeOrder,
    /// Absolute edge slot index per value chunk.
    pub slot_indices: &'a [u32],
    /// Flattened payload bytes in the same order as `slot_indices`.
    pub values: &'a [u8],
    /// `true` when values were read from a contiguous resident payload slab span.
    pub dense: bool,
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
    attach_payload: bool,
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
                        true,
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
                true,
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
        let edge = edge.with_label_id(self.label_id.raw());
        if self.attach_payload {
            Some(self.graph.attach_edge_payload(
                self.src,
                &self.vertex,
                self.bucket_index,
                self.bucket,
                slot,
                edge,
                self.log_chains.as_ref(),
            ))
        } else {
            Some(Ok(edge))
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
        attach_payload: bool,
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
            attach_payload,
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
        attach_payload: bool,
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
            attach_payload,
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
