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
use super::error::LabeledOperationError;

/// See the module documentation.
pub(crate) struct BucketTreePlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
    bucket: LabelBucket,
}

/// Where a tree's edge-root array lives.
///
/// ADR 0101 put the edge root in the leaf's edge slots and the value root in the value store. A bucket whose
/// vertex slice has no room left cannot do that - and the alternative of moving the edge root out of the edge
/// side is refused - so the edge root may instead live in an **edge spill run**: the same arena edge rows use for
/// their overflow. The two cases are one type, so a caller asks this view rather than reading
/// `bucket.edge_start()`.
///
/// A tree bucket's rows always live in LTB blocks, never in a run, so "tree bucket carries an edge run" can only
/// mean the root array - that is what makes the descriptor need no new field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeRootLoc {
    /// The root array occupies `slots` edge slots starting at `start`.
    Slots { start: u64, slots: u32 },
    /// The root array occupies `rows` rows of `run`, one block id per row.
    Run {
        run: crate::labeled::spill_run_store::SpillRunId,
        rows: u32,
    },
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

    /// Where this tree's edge-root array is, and how long it is.
    pub(crate) fn edge_root_loc(&self) -> EdgeRootLoc {
        if self.bucket.is_tree_mode()
            && let Ok(run) = self.graph.edges().spill_mint(self.bucket.edge_spill_run())
        {
            let rows = self.graph.edges().spill_used_len(run);
            return EdgeRootLoc::Run { run, rows };
        }
        let (start, slots) = self.edge_root();
        EdgeRootLoc::Slots { start, slots }
    }

    /// Block id `index` of this tree's edge root, from whichever store holds the array.
    pub(crate) fn edge_child(&self, index: u32) -> Result<u32, LabeledOperationError> {
        let mut id_bytes = [0u8; 4];
        match self.edge_root_loc() {
            EdgeRootLoc::Slots { start, .. } => {
                self.graph
                    .edges()
                    .read_slot_bytes(start + u64::from(index), &mut id_bytes);
            }
            EdgeRootLoc::Run { run, rows } => {
                if index >= rows {
                    return Err(LabeledOperationError::Store(
                        crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow,
                    ));
                }
                self.graph
                    .edges()
                    .spill_read_row(run, index, &mut id_bytes)
                    .map_err(LabeledOperationError::from)?;
            }
        }
        Ok(u32::from_le_bytes(id_bytes))
    }

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
        // Whether a value root exists is a question the geometry answers, not the recorded address: offset 0 is
        // also the first span's address, so treating it as "no root" leaked that span.
        if replaced > 0 && previous != offset {
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
            .map_err(LabeledOperationError::from)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::test_support::{InlinePropertyTestEdge, inline_property_test_graph};

    type Graph = LabeledLaraGraph<InlinePropertyTestEdge, VectorMemory>;

    fn label() -> BucketLabelKey {
        BucketLabelKey::directed_from_index(3)
    }

    /// Installs a depth-1 tree bucket whose roots name the given children.
    fn install(graph: &Graph, edge_children: &[u32], value_children: &[u32]) -> LabelBucket {
        crate::labeled::graph::test_support::install_tree_bucket::<
            InlinePropertyTestEdge,
            VectorMemory,
        >(
            graph,
            0,
            label(),
            4096,
            4,
            1,
            1,
            edge_children,
            value_children,
        )
    }

    /// A tree bucket's edge root is answered as a location, and its children read through that location.
    ///
    /// The location is the type that lets a root live in the leaf slots (ADR 0101) or in an edge run (a vertex slice
    /// with no room left); today every tree bucket takes the slot form, and the run form is what the promotion adds.
    #[test]
    fn the_edge_root_is_a_location_and_its_children_read_through_it() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23]);
        let plane = graph.tree_plane(bucket);
        let (start, slots) = plane.edge_root();
        assert_eq!(
            plane.edge_root_loc(),
            EdgeRootLoc::Slots { start, slots },
            "a tree bucket with a leaf span keeps its root in the leaf"
        );
        for (index, expected) in [11u32, 12, 13, 14].into_iter().enumerate() {
            assert_eq!(
                plane
                    .edge_child(u32::try_from(index).expect("fits"))
                    .expect("read"),
                expected,
                "edge root child {index}"
            );
        }
    }

    /// ADR 0101: the descriptor records where the value root went, and both roots read back what was written.
    #[test]
    fn publish_places_each_root_in_its_own_store() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23]);
        // The edge root is the leaf's slot array; 4096 rows at 1024 rows per leaf need four leaves.
        let (edge_start, edge_slots) = graph.tree_plane(bucket).edge_root();
        assert_eq!(edge_start, bucket.edge_start());
        assert_eq!(edge_slots, 4, "the root region is one slot per leaf");
        // The value root is a block-address array in the value store.
        let (offset, bytes) = graph.tree_plane(bucket).value_root();
        assert_eq!(bytes, 4 * 4, "four children mean sixteen bytes");
        assert_eq!(offset, bucket.inline_property_bytes_offset());
        assert_eq!(
            graph.tree_plane(bucket).value_child(1),
            22,
            "each value child reads back from the value store"
        );
    }

    /// A publication that changes neither root publishes the descriptor alone - the reachable case where an
    /// append stays inside the leaf tree it already has.
    #[test]
    fn publish_with_no_images_keeps_the_roots_it_had() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23, 24]);
        let published = graph
            .tree_plane(bucket)
            .publish::<InlinePropertyTestEdge>(0, |b| b.with_degree_field(7), 0, &[], &[])
            .expect("publish");
        assert_eq!(published.degree, 7, "the shape is applied");
        assert_eq!(
            published.inline_property_bytes_offset(),
            bucket.inline_property_bytes_offset(),
            "an empty image leaves the value root where it was"
        );
        assert_eq!(graph.tree_plane(published).value_child(0), 21);
    }

    /// `shape` states the descriptor's shape; the plane states where the value root went. A call site that
    /// builds its descriptor before the write still gets the recorded address.
    #[test]
    fn publish_applies_the_value_root_offset_after_the_shape() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23, 24]);
        // A shape built from a descriptor that knows nothing about the value root.
        let pre_built = bucket.with_inline_property_bytes_offset(0);
        let published = graph
            .tree_plane(bucket)
            .publish::<InlinePropertyTestEdge>(
                0,
                |_| pre_built,
                bucket.edge_start(),
                &[],
                &[31, 32],
            )
            .expect("publish");
        assert_eq!(
            graph.tree_plane(published).value_child(1),
            32,
            "the plane's address wins over whatever the shape carried"
        );
    }

    /// Replacing the value root gives the old span back, so a growing tree does not leak its previous root.
    #[test]
    fn ensure_value_root_releases_the_span_it_replaces() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23, 24]);
        let before = bucket.inline_property_bytes_offset();
        let replaced = graph.tree_plane(bucket).value_root().1;
        let grown = graph
            .tree_plane(bucket)
            .ensure_value_root(&[21, 22, 23, 24, 25])
            .expect("grow the value root");
        assert_ne!(grown, before, "a wider root needs a wider span");
        assert_eq!(graph.tree_plane(bucket).value_child(0), 21);
        // The replaced region is accounted as free again rather than staying charged to the bucket.
        let stats = graph.values().allocator_stats();
        assert!(
            stats.free_bytes >= u64::from(replaced)
                || stats.slab_occupied_tail <= u64::from(replaced) + 20,
            "the replaced span must be free or reclaimed: {stats:?} (before={before}, replaced={replaced})"
        );
    }

    /// The packed reader and the per-child reader answer the same array.
    #[test]
    fn value_children_returns_the_packed_array() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23, 24]);
        let mut expected = Vec::new();
        for id in [21u32, 22, 23, 24] {
            expected.extend_from_slice(&id.to_le_bytes());
        }
        assert_eq!(graph.tree_plane(bucket).value_children(4), expected);
    }

    /// Writing the edge root pads to whole slots, so a byte image that ends mid-slot still lands as one row.
    #[test]
    fn write_edge_root_pads_to_a_whole_slot() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23, 24]);
        let start = graph
            .edges()
            .allocate_span(1)
            .expect("allocate a scratch slot");
        graph
            .tree_plane(bucket)
            .write_edge_root::<InlinePropertyTestEdge>(start, &7u32.to_le_bytes()[..3])
            .expect("write a short image");
        let mut row = [0u8; 4];
        graph.edges().read_slots_contiguous_bytes(start, &mut row);
        assert_eq!(row, [7, 0, 0, 0], "the unused tail of the slot is zeroed");
    }
}
