//! Vertex-column accessors for labeled CSR stores.

use crate::{
    VertexId,
    labeled::{bucket_store::LabelBucketStore, record::LabelBucket},
    lara::operation_error::VertexAccess,
};
use ic_stable_structures::Memory;

/// Presents one global bucket slab slot as a tiny vertex column for
/// [`crate::lara::edge::EdgeStore`].
///
/// Row `0` is the live bucket. Row `1` is a synthetic successor whose base is
/// supplied by the owning VertexEdgeSpan. This gives `EdgeStore` the
/// same CSR successor-boundary geometry it uses for normal vertex rows while
/// allowing optional per-label overflow into the shared segment log.
///
/// The caller must calculate `successor_start` from the sorted LabelBucket range:
/// either the next bucket's `edge_start`, or the containing VertexEdgeSpan
/// end for the last bucket. Pass `log_vertex` as the graph source [`VertexId`]
/// so overflow log entries and PMA bumps use the correct PMA leaf.
pub struct LabelEdgeSpanAccess<'a, M: Memory> {
    buckets: &'a LabelBucketStore<M>,
    slot: u64,
    successor_start: u64,
    log_vertex: VertexId,
}

impl<'a, M: Memory> LabelEdgeSpanAccess<'a, M> {
    /// Binds EdgeStore scan helpers to the LabelEdgeSpan described by the bucket at `slot`.
    pub fn new(
        buckets: &'a LabelBucketStore<M>,
        slot: u64,
        successor_start: u64,
        log_vertex: VertexId,
    ) -> Self {
        Self {
            buckets,
            slot,
            successor_start,
            log_vertex,
        }
    }
}

impl<M: Memory> VertexAccess<LabelBucket> for LabelEdgeSpanAccess<'_, M> {
    fn log_leaf_vertex(&self, _id: VertexId) -> VertexId {
        self.log_vertex
    }

    fn len(&self) -> u32 {
        2
    }

    fn get(&self, id: VertexId) -> LabelBucket {
        let bucket = self
            .buckets
            .read_label_bucket_slot(self.slot)
            .unwrap_or_default();
        match u32::from(id) {
            0 => bucket,
            1 => {
                let succ = self.successor_start.max(bucket.edge_start);
                bucket.with_edge_range(succ, 0).with_overflow_log_head(-1)
            }
            _ => panic!("LabelEdgeSpanAccess only exposes row 0 and successor row 1"),
        }
    }

    fn set(&self, id: VertexId, item: &LabelBucket) {
        debug_assert_eq!(u32::from(id), 0);
        if u32::from(id) != 0 {
            return;
        }
        self.buckets
            .write_label_bucket_slot(self.slot, *item)
            .expect("LabelBucket write failed");
    }
}
