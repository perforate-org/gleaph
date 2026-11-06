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

    /// Writes a tree bucket's two roots beginning at `edge_start` and returns the image it wrote (for
    /// rollback).
    ///
    /// The caller hands over what it wants in each root - the edge root's slot image and the value tree's child
    /// addresses - and this composes them for the layout that is stored. While both roots share the edge span
    /// the composition is `edge ++ children`, padded to whole slots; ADR 0101's switch replaces the composition
    /// here rather than at each of the six write sites.
    pub(crate) fn write_roots<E2>(
        &self,
        edge_start: u64,
        edge_root_image: &[u8],
        value_children: &[u32],
    ) -> Result<Vec<u8>, super::LabeledOperationError>
    where
        E2: CsrEdge + crate::traits::CsrEdgeTombstone,
    {
        let slot_bytes = E2::BYTES;
        let children_bytes = value_children.len() * 4;
        let total = edge_root_image.len().saturating_add(children_bytes);
        let slots = total.div_ceil(slot_bytes.max(1));
        let mut image = vec![0u8; slots * slot_bytes];
        image[..edge_root_image.len()].copy_from_slice(edge_root_image);
        for (index, child) in value_children.iter().enumerate() {
            let offset = edge_root_image.len() + index * 4;
            image[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
        }
        self.graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &image)
            .map_err(|_| {
                super::LabeledOperationError::from(
                    crate::lara::operation_error::LaraOperationError::overflow(),
                )
            })?;
        Ok(image)
    }

    /// Writes a tree bucket's roots and publishes its descriptor: the only way a tree bucket is published.
    ///
    /// The caller hands over only the *shape* it wants (edge range, degree, stored slots, depths). The root
    /// layout - which store each root lives in, in which order they are written, and how the descriptor records
    /// the result - stays here, so a call site cannot publish a descriptor that disagrees with the roots it
    /// just wrote.
    pub(crate) fn publish<E2>(
        &self,
        bucket_slot: u64,
        shape: impl FnOnce(LabelBucket) -> LabelBucket,
        edge_start: u64,
        edge_root_image: &[u8],
        value_children: &[u32],
    ) -> Result<LabelBucket, super::LabeledOperationError>
    where
        E2: CsrEdge + crate::traits::CsrEdgeTombstone,
    {
        // While both roots share the edge span the stored image is `edge ++ children`, padded to slots; the
        // ADR 0101 split replaces this composition and nothing else.
        // An empty image means the caller is not changing the roots (an append that stays inside the
        // existing leaf tree); only the descriptor is published then.
        if edge_root_image.is_empty() && value_children.is_empty() {
            let published = shape(self.bucket);
            self.graph
                .buckets()
                .write_label_bucket_slot(bucket_slot, published)
                .map_err(super::LabeledOperationError::from)?;
            return Ok(published);
        }
        let slot_bytes = E2::BYTES;
        let children_bytes = value_children.len() * 4;
        let total = edge_root_image.len() + children_bytes;
        let slots = total.div_ceil(slot_bytes.max(1));
        let mut image = vec![0u8; slots * slot_bytes];
        image[..edge_root_image.len()].copy_from_slice(edge_root_image);
        for (index, child) in value_children.iter().enumerate() {
            let offset = edge_root_image.len() + index * 4;
            image[offset..offset + 4].copy_from_slice(&child.to_le_bytes());
        }
        self.graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &image)
            .map_err(super::LabeledOperationError::from)?;
        let published = shape(self.bucket);
        self.graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, published)
            .map_err(super::LabeledOperationError::from)?;
        Ok(published)
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
