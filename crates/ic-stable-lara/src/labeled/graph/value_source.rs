//! Where a bucket's inline property values are read from.
//!
//! ADR 0100's remaining View family is the one that reads rows *with* their values. Its duplication is not in
//! how the rows are walked - that is `visit_rows` now - but in the three answers to "read slot `s`'s value":
//! a tiny bucket has none, a slab bucket reads its value span or run, a tree bucket reads its property tree.
//!
//! Every with-values entry was spelling that choice itself, which is why one of them carried eight mode
//! branches. [`BucketValueSource`] answers it once: resolve from the bucket, then read slots.

use crate::traits::CsrEdge;

use super::super::record::LabelBucket;
use super::LabeledLaraGraph;
use ic_stable_structures::Memory;

/// Where one bucket's inline property values live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BucketValueSource {
    /// The bucket has no inline property values at this width.
    None,
    /// The values live in the inline property bytes store, addressed by slot.
    Slab {
        /// The bucket's value width in bytes.
        width: u16,
    },
    /// The values live in the property tree, addressed by slot.
    Tree {
        /// The bucket's value width in bytes.
        width: u16,
    },
}

impl BucketValueSource {
    /// Reads the source a bucket uses.
    ///
    /// This is the one place that turns storage class into a value address space; a caller that reads values
    /// asks this rather than deciding for itself.
    pub(crate) fn of(bucket: &LabelBucket) -> Self {
        use super::BucketMode;
        let width = bucket.inline_property_byte_width();
        match BucketMode::from_bucket(bucket) {
            // Tiny schema bytes are target payload, including native tombstone encodings, so they are never a
            // value width.
            BucketMode::Tiny => Self::None,
            BucketMode::Slab => {
                if width == 0 {
                    Self::None
                } else {
                    Self::Slab { width }
                }
            }
            BucketMode::Tree => {
                if width == 0 {
                    Self::None
                } else {
                    Self::Tree { width }
                }
            }
        }
    }

    /// The width this source reports, or zero when there is nothing to read.
    pub(crate) const fn width(self) -> u16 {
        match self {
            Self::None => 0,
            Self::Slab { width } | Self::Tree { width } => width,
        }
    }

    /// Reads one slot's value bytes into `out`.
    ///
    /// `out` is resized to the source's width; a source with nothing to read clears it and returns.
    pub(crate) fn read_slot_into<E, M>(
        self,
        graph: &LabeledLaraGraph<E, M>,
        bucket: &LabelBucket,
        slot_index: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), super::LabeledOperationError>
    where
        E: CsrEdge,
        M: Memory,
    {
        let width = usize::from(self.width());
        out.clear();
        out.resize(width, 0);
        if width == 0 {
            return Ok(());
        }
        match self {
            Self::None => Ok(()),
            Self::Tree { .. } => {
                super::tree_read::read_property_value_at_slot(graph, bucket, slot_index, out)
            }
            Self::Slab { .. } => {
                graph.read_bucket_inline_property_bytes_for_slot_into(bucket, slot_index, None, out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::EdgePlacementPolicy;
    use crate::labeled::graph::test_support::{TestEdge, test_graph};
    use crate::labeled::record::LabeledVertex;

    type Graph = LabeledLaraGraph<TestEdge, VectorMemory>;

    fn label() -> BucketLabelKey {
        BucketLabelKey::directed_from_index(2)
    }

    /// A bucket with no declared width has no value source, whatever storage class holds it.
    #[test]
    fn a_bucket_without_a_width_has_no_source() {
        let graph = test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        for target in 100..104u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label()).expect("find") {
            crate::labeled::graph::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        let source = BucketValueSource::of(&bucket);
        assert_eq!(source, BucketValueSource::None);
        assert_eq!(source.width(), 0);
        let mut out = vec![9u8; 4];
        source
            .read_slot_into(&graph, &bucket, 0, &mut out)
            .expect("read");
        assert!(
            out.is_empty(),
            "a source with nothing to read clears the buffer"
        );
    }

    /// A slab bucket answers both questions the source exists for: which class reads it, and what a slot holds.
    #[test]
    fn a_slab_bucket_reads_the_value_at_the_slot_it_is_asked_for() {
        use crate::labeled::graph::test_support::{
            InlinePropertyTestEdge, inline_property_test_graph,
        };
        let graph = inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let width = 2u16;
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label(), width)
            .expect("declare the width");
        for target in 100..108u32 {
            let value = u16::try_from(target).expect("fits").to_le_bytes();
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &value),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label()).expect("find") {
            crate::labeled::graph::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        let source = BucketValueSource::of(&bucket);
        assert_eq!(source, BucketValueSource::Slab { width });
        let mut out = Vec::new();
        for slot in 0..8u32 {
            source
                .read_slot_into(&graph, &bucket, slot, &mut out)
                .expect("read a slot");
            assert_eq!(
                out,
                u16::try_from(100 + slot).expect("fits").to_le_bytes(),
                "slot {slot} reads its own value, not a neighbour's"
            );
        }
    }
}
