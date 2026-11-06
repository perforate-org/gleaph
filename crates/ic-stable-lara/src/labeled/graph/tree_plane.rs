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
        let mut id_bytes = [0u8; 4];
        self.graph.values().read_bytes(
            compact::value_root_address(&self.bucket) + u64::from(index) * 4,
            &mut id_bytes,
        );
        u32::from_le_bytes(id_bytes)
    }

    /// The first `count` child addresses of the value tree, packed as they are stored.
    pub(crate) fn value_children(&self, count: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; count as usize * 4];
        if !bytes.is_empty() {
            self.graph
                .values()
                .read_bytes(compact::value_root_address(&self.bucket), &mut bytes);
        }
        bytes
    }

    /// Admits the value root's region in the inline property bytes store and writes its child addresses
    /// there (ADR 0101). Returns the offset the descriptor must record.
    pub(crate) fn ensure_value_root(
        &self,
        value_children: &[u32],
    ) -> Result<u64, super::LabeledOperationError> {
        let mut children = Vec::with_capacity(value_children.len() * 4);
        for child in value_children {
            children.extend_from_slice(&child.to_le_bytes());
        }
        let replaced = compact::value_root_region_bytes(&self.bucket);
        let previous = self.bucket.inline_property_bytes_offset();
        let offset = self
            .graph
            .values()
            .allocate_byte_span(children.len() as u64)
            .map_err(|_| {
                super::LabeledOperationError::from(
                    crate::lara::operation_error::LaraOperationError::overflow(),
                )
            })?;
        self.graph
            .values()
            .write_bytes(offset, &children)
            .map_err(|_| {
                super::LabeledOperationError::from(
                    crate::lara::operation_error::LaraOperationError::overflow(),
                )
            })?;
        if replaced > 0 && previous != offset && self.bucket.tree_mode_property_depth() > 0 {
            let _ = self
                .graph
                .values()
                .release_byte_span(previous, u64::from(replaced));
        }
        Ok(offset)
    }

    /// Writes the edge root's slot image at `edge_start`.
    pub(crate) fn write_edge_root<E2>(
        &self,
        edge_start: u64,
        edge_root_image: &[u8],
    ) -> Result<(), super::LabeledOperationError>
    where
        E2: CsrEdge + crate::traits::CsrEdgeTombstone,
    {
        let slot_bytes = E2::BYTES;
        let slots = edge_root_image.len().div_ceil(slot_bytes.max(1));
        let mut image = vec![0u8; slots * slot_bytes];
        image[..edge_root_image.len()].copy_from_slice(edge_root_image);
        self.graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &image)
            .map_err(super::LabeledOperationError::from)
    }

    /// Writes both roots and returns the value root's new offset (for the paths whose publication is not this
    /// view's: promotion publishes through its own guard).
    pub(crate) fn write_roots<E2>(
        &self,
        edge_start: u64,
        edge_root_image: &[u8],
        value_children: &[u32],
    ) -> Result<u64, super::LabeledOperationError>
    where
        E2: CsrEdge + crate::traits::CsrEdgeTombstone,
    {
        let offset = if value_children.is_empty() {
            0
        } else {
            self.ensure_value_root(value_children)?
        };
        self.write_edge_root::<E2>(edge_start, edge_root_image)?;
        Ok(offset)
    }

    /// Writes a tree bucket's roots and publishes its descriptor: the only way a tree bucket is published.
    ///
    /// The caller states the shape it wants (edge range, degree, stored slots, depths) as a function of the
    /// descriptor. Where each root lives, in which order they are written, and how the descriptor records the
    /// value root's address stay here - so a call site cannot publish a descriptor that disagrees with the roots
    /// it just wrote, and it does not have to know that the value root moved at all.
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
        let mut published = shape(self.bucket);
        if !edge_root_image.is_empty() {
            self.write_edge_root::<E2>(edge_start, edge_root_image)?;
        }
        if !value_children.is_empty() {
            let offset = self.ensure_value_root(value_children)?;
            published = published.with_inline_property_bytes_offset(offset);
        }
        self.graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, published)
            .map_err(super::LabeledOperationError::from)?;
        Ok(published)
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
