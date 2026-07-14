//! Vertex-column accessors for labeled CSR stores.

use super::bucket_store::LabelBucketStore;
use crate::{VertexId, labeled::record::LabelBucket, lara::operation_error::VertexAccess};
use ic_stable_structures::Memory;
use std::cell::Cell;

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
pub(crate) struct LabelEdgeSpanAccess<'a, M: Memory> {
    buckets: &'a LabelBucketStore<M>,
    slot: u64,
    bucket: Cell<LabelBucket>,
    successor_start: u64,
    log_vertex: VertexId,
}

impl<'a, M: Memory> LabelEdgeSpanAccess<'a, M> {
    /// Binds a resolved bucket to the CSR adapter without duplicating its stable-memory read.
    pub(crate) fn with_bucket(
        buckets: &'a LabelBucketStore<M>,
        slot: u64,
        bucket: LabelBucket,
        successor_start: u64,
        log_vertex: VertexId,
    ) -> Self {
        Self {
            buckets,
            slot,
            bucket: Cell::new(bucket),
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
        let bucket = self.bucket.get();
        match u32::from(id) {
            0 => bucket,
            1 => {
                // For a log-backed bucket with no on-slab prefix, present a zero-width
                // slab window so `EdgeStore::insert_edge` routes new edges into the
                // shared overflow log instead of writing into the placeholder slab
                // region. The full successor boundary is still used by scan paths
                // once the bucket is folded to slab.
                let succ = if bucket.overflow_log_head() >= 0 && bucket.stored_slots == 0 {
                    bucket.edge_start()
                } else {
                    self.successor_start.max(bucket.edge_start())
                };
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
        self.bucket.set(*item);
        self.buckets
            .write_label_bucket_slot(self.slot, *item)
            .expect("LabelBucket write failed");
    }
}
