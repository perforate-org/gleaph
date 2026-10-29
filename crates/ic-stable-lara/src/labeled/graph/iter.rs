//! Labeled outgoing-edge iterators.

use crate::{
    VertexId,
    labeled::{bucket_label_key::BucketLabelKey, record::LabelBucket},
    traits::{CsrEdgeTombstone, CsrVertex},
};
use ic_stable_structures::Memory;
use std::{cell::Cell, iter::FusedIterator, num::NonZero, ops::ControlFlow, rc::Rc};

use super::{LabeledLaraGraph, LabeledOperationError, OutEdgeOrder};

/// Reusable buffers for labeled edge-inline-property-bytes batch traversal.
#[derive(Clone, Debug)]
pub struct LabeledEdgeInlinePropertyBatchScratch<E> {
    /// Edge rows in the same order as the parallel value byte chunks.
    pub edges: Vec<E>,
    /// Flattened value bytes: `edges.len() * batch.byte_width.byte_width()` bytes.
    pub inline_property_bytes: Vec<u8>,
    /// Reusable bulk-read buffer for contiguous edge slab IO.
    pub(super) io_edge_bytes: Vec<u8>,
    /// Reusable bulk-read buffer for contiguous inline property bytes slab IO.
    pub(super) io_inline_property_bytes: Vec<u8>,
    pub(super) stop_requested: Rc<Cell<bool>>,
}

impl<E> Default for LabeledEdgeInlinePropertyBatchScratch<E> {
    fn default() -> Self {
        Self {
            edges: Vec::new(),
            inline_property_bytes: Vec::new(),
            io_edge_bytes: Vec::new(),
            io_inline_property_bytes: Vec::new(),
            stop_requested: Rc::new(Cell::new(false)),
        }
    }
}

impl<E> LabeledEdgeInlinePropertyBatchScratch<E> {
    /// Clears both reusable buffers while preserving allocation capacity.
    pub fn clear(&mut self) {
        self.edges.clear();
        self.inline_property_bytes.clear();
    }

    pub(super) fn reset_stop(&mut self) {
        self.stop_requested.set(false);
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

    pub(super) fn io_inline_property_bytes_slice_mut(&mut self, len: usize) -> &mut [u8] {
        if self.io_inline_property_bytes.len() < len {
            self.io_inline_property_bytes.resize(len, 0);
        }
        &mut self.io_inline_property_bytes[..len]
    }

    pub(super) fn io_inline_property_bytes_slice(&self, len: usize) -> &[u8] {
        &self.io_inline_property_bytes[..len]
    }
}

/// Reusable buffers for inline-property-bytes-only batch traversal (phase 1).
#[derive(Clone, Debug, Default)]
pub struct LabeledInlinePropertyValueBatchScratch {
    /// Absolute edge slot index per value chunk in `values`.
    pub slot_indices: Vec<u32>,
    /// Flattened inline property bytes: `slot_indices.len() * byte_width` when emitted.
    pub values: Vec<u8>,
    /// Reusable bulk-read buffer for contiguous inline property bytes slab IO.
    pub(super) io_inline_property_bytes: Vec<u8>,
}

impl LabeledInlinePropertyValueBatchScratch {
    /// Clears batch buffers while retaining their allocation.
    pub fn clear(&mut self) {
        self.slot_indices.clear();
        self.values.clear();
    }

    pub(super) fn io_inline_property_bytes_slice_mut(&mut self, len: usize) -> &mut [u8] {
        if self.io_inline_property_bytes.len() < len {
            self.io_inline_property_bytes.resize(len, 0);
        }
        &mut self.io_inline_property_bytes[..len]
    }
}

/// One batch of parallel inline property bytes for a single label bucket (no edge rows).
pub struct LabeledInlinePropertyValueBatch<'a> {
    /// Label bucket visited by this batch.
    pub label_id: BucketLabelKey,
    /// Physical byte width of each inline property bytes chunk in `values`.
    pub byte_width: u16,
    /// Scan order used for `slot_indices` and `values`.
    pub order: OutEdgeOrder,
    /// Absolute edge slot index per value chunk.
    pub slot_indices: &'a [u32],
    /// Flattened inline property bytes in the same order as `slot_indices`.
    pub values: &'a [u8],
    /// `true` when values were read from a contiguous resident inline property bytes slab span.
    pub dense: bool,
}

/// One batch of edges and their parallel value bytes for a single label bucket.
pub struct LabeledEdgeInlinePropertyBatch<'a, E> {
    /// Label bucket visited by this batch.
    pub label_id: BucketLabelKey,
    /// Physical byte width of each edge's inline property bytes in this batch.
    pub byte_width: u16,
    /// Scan order used for both `edges` and `inline_property_bytes`.
    pub order: OutEdgeOrder,
    /// Edge rows in scan order.
    pub edges: &'a [E],
    /// Flattened edge-inline-property-bytes bytes in the same order as `edges`.
    pub inline_property_bytes: &'a [u8],
    /// `true` when the batch was read from contiguous resident edge/inline property bytes slab spans.
    pub dense: bool,
}

/// Streaming iterator over outgoing edges in a fixed scan order.
///
/// Items are fallible because labeled edge rows may need to read inline property bytes from stable
/// inline property bytes storage. Use `collect::<Result<Vec<_>, _>>()` when materializing rows. For paging or
/// Labeled outgoing-edge iterator that materializes one bucket at a time.
///
/// The iterator enumerates the requested label buckets once at construction and then
/// lazily fills its edge buffer by delegating each bucket to
/// [`super::traverse::LabeledLaraGraph::visit_edges_with_inline_property`].
/// Storage errors therefore surface during iteration, matching the legacy streaming
/// contract.
pub struct LabeledOutEdgesIter<'a, E: CsrEdgeTombstone, M: Memory> {
    graph: &'a LabeledLaraGraph<E, M>,
    src: VertexId,
    order: OutEdgeOrder,
    buckets: Vec<LabelBucket>,
    base_bucket_index: u32,
    next_bucket_local: Option<usize>,
    current: std::vec::IntoIter<E>,
}

impl<'a, E, M> Iterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    type Item = Result<E, LabeledOperationError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(edge) = self.current.next() {
                return Some(Ok(edge));
            }
            let local = self.next_bucket_local?;
            if self.order == OutEdgeOrder::Descending {
                self.next_bucket_local = local.checked_sub(1);
            } else {
                let next = local + 1;
                self.next_bucket_local = (next < self.buckets.len()).then_some(next);
            }
            if self.buckets[local].degree() == 0 {
                continue;
            }
            let bucket = &self.buckets[local];
            let label_id = bucket.bucket_label_key();
            let mut buf = Vec::new();
            let result = self.graph.visit_edges_with_inline_property_unchecked(
                self.src,
                label_id,
                self.order,
                |_slot, item| {
                    let edge = item
                        .edge
                        .with_stored_inline_property_bytes(
                            item.inline_property.width(),
                            item.inline_property.bytes(),
                        )
                        .with_label_id(label_id.raw());
                    buf.push(edge);
                    ControlFlow::<()>::Continue(())
                },
            );
            if let Err(err) = result {
                self.next_bucket_local = None;
                return Some(Err(err));
            }
            self.current = buf.into_iter();
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

    fn count(self) -> usize {
        self.collect::<Vec<_>>().len()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Lower bound is the already-buffered edges; upper bound is unknown because
        // each remaining bucket may yield up to its degree.
        let buffered = self.current.len();
        (buffered, None)
    }
}

impl<'a, E, M> FusedIterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
}

impl<'a, E, M> LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdgeTombstone,
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
            buckets: Vec::new(),
            base_bucket_index: 0,
            next_bucket_local: None,
            current: Vec::new().into_iter(),
        }
    }

    pub(super) fn from_buckets(
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        order: OutEdgeOrder,
        base_bucket_index: u32,
        buckets: Vec<LabelBucket>,
    ) -> Self {
        let next_bucket_local = match order {
            OutEdgeOrder::Descending => buckets.len().checked_sub(1),
            OutEdgeOrder::Ascending => (!buckets.is_empty()).then_some(0),
        };
        Self {
            graph,
            src,
            order,
            buckets,
            base_bucket_index,
            next_bucket_local,
            current: Vec::new().into_iter(),
        }
    }

    /// Advances by up to `n` live rows.
    ///
    /// Returns `Ok(Err(left))` when the iterator ends before all `n` rows are skipped,
    /// or `Err` if a storage/layout error is encountered while materializing a bucket.
    pub fn try_advance_by(
        &mut self,
        mut n: usize,
    ) -> Result<Result<(), NonZero<usize>>, LabeledOperationError> {
        if n == 0 {
            return Ok(Ok(()));
        }
        loop {
            let buffered = self.current.len();
            if n <= buffered {
                self.current.nth(n - 1);
                return Ok(Ok(()));
            }
            n -= buffered;
            self.current = Vec::new().into_iter();
            match self.next() {
                Some(Ok(_)) => {
                    n -= 1;
                    if n == 0 {
                        return Ok(Ok(()));
                    }
                }
                Some(Err(err)) => return Err(err),
                None => return Ok(Err(NonZero::new(n).expect("n > 0"))),
            }
        }
    }
}
