//! One tree bucket's roots, borrowed for one operation.
//!
//! ADR 0102. A tree bucket has two roots - one over edges, one over inline property values - and today six
//! independent functions write them, each building its own image, ordering its own descriptor publication and
//! carrying its own rollback. This view is the owner: the write entry (added in the next cut) publishes both
//! roots and then the descriptor, once, and the store-level helpers move inside this module so a sixth path
//! cannot appear by copying a sibling.
//!
//! This first cut takes over the **read** side, because "which store holds which root" is the same question
//! in both directions: the child addresses of the value tree are answered here and nowhere else, so a caller
//! cannot independently decide that they live in the edge span.

use crate::traits::CsrEdge;
use ic_stable_structures::Memory;

use super::LabelBucket;
use super::LabeledLaraGraph;
use super::compact::{self, TreeSpans};

/// See the module documentation.
pub(crate) struct BucketTreePlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
    bucket: LabelBucket,
}

impl<'a, E, M> BucketTreePlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) const fn new(graph: &'a LabeledLaraGraph<E, M>, bucket: LabelBucket) -> Self {
        Self { graph, bucket }
    }

    /// The descriptor this view answers for.
    pub(crate) const fn bucket(&self) -> LabelBucket {
        self.bucket
    }

    /// The edge root's span: `(start_slot, slots)` in the leaf.
    pub(crate) fn edge_root(&self) -> (u64, u32) {
        TreeSpans::of(&self.bucket)
            .map(|spans| spans.edge_root())
            .unwrap_or((self.bucket.edge_start(), 0))
    }

    /// The value root's region in the inline property bytes store: `(byte_offset, bytes)`.
    pub(crate) fn value_root(&self) -> (u64, u32) {
        (
            compact::value_root_address(&self.bucket),
            compact::value_root_region_bytes(&self.bucket),
        )
    }

    /// Child address `index` of the value tree.
    ///
    /// The **stored** layout still keeps both roots in the edge span, so this reads the combined geometry.
    /// ADR 0101's switch changes this method and `value_children` - inside this module, which is ADR 0102's
    /// point: the callers stay as they are.
    pub(crate) fn value_child(&self, index: u32) -> u32 {
        let spans = TreeSpans::of(&self.bucket).expect("tree-mode bucket has root spans");
        let (slot, byte_offset) = spans.property_root_entry(index);
        let mut id_bytes = [0u8; 4];
        self.graph
            .edges()
            .read_from_slot_bytes(slot, byte_offset, &mut id_bytes);
        u32::from_le_bytes(id_bytes)
    }

    /// The first `count` child addresses of the value tree, packed as they are stored.
    pub(crate) fn value_children(&self, count: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; count as usize * 4];
        if !bytes.is_empty() {
            let spans = TreeSpans::of(&self.bucket).expect("tree-mode bucket has root spans");
            let (slot, byte_offset) = spans.property_root_entry(0);
            self.graph
                .edges()
                .read_from_slot_bytes(slot, byte_offset, &mut bytes);
        }
        bytes
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The tree plane of one bucket (ADR 0102: the only way to a tree's roots).
    pub(crate) fn tree_plane(&self, bucket: LabelBucket) -> BucketTreePlane<'_, E, M> {
        BucketTreePlane::new(self, bucket)
    }
}
