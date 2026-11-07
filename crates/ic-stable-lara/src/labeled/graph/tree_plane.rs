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

/// How many entries each of a tree bucket's root arrays holds.
///
/// Both roots answer the same question - "one id per child at the level below" - and both used to answer it with their
/// own arithmetic at every call site: the edge root through `tree_csr::root_len(stored_slots)`, the value root through
/// `property_root_entry_count(leaves, depth)`. Writing a root, reading one back and validating one must all agree, so the
/// pair is derived here once and everyone asks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TreeRootEntries {
    /// Ids the edge root array holds.
    pub(crate) edge: u32,
    /// Ids the value root array holds.
    pub(crate) value: u32,
}

/// The root shapes of `bucket` at `property_slots_per_block`.
pub(crate) fn tree_root_entries(bucket: &LabelBucket, slots_per_block: u32) -> TreeRootEntries {
    let edge = u32::try_from(crate::labeled::tree_csr::root_len(bucket.stored_slots()))
        .unwrap_or(u32::MAX);
    let value = if !bucket.is_tree_mode() || bucket.inline_property_byte_width() == 0 {
        0
    } else {
        let leaves =
            u64::from(bucket.stored_slots().max(1)).div_ceil(u64::from(slots_per_block.max(1)));
        u32::try_from(super::tree_write::property_root_entry_count(
            leaves,
            bucket.tree_mode_property_depth(),
        ))
        .unwrap_or(u32::MAX)
    };
    TreeRootEntries { edge, value }
}

/// Refuses a root array whose shape is not the bucket's, or whose ids are not distinct.
///
/// This is the check the edge root has always satisfied by construction ("root length == blocks that exist") and the
/// value root did not: a root that holds the wrong number of ids, or the same id twice, publishes a tree that reads
/// another row's payload while looking valid.
pub(crate) fn validate_root_ids(
    root: &'static str,
    expected: u32,
    ids: &[u32],
) -> Result<(), LabeledOperationError> {
    if ids.len() != expected as usize {
        return Err(LabeledOperationError::RootEntryCountMismatch(
            super::error::RootEntryCountMismatch {
                root,
                expected,
                got: u32::try_from(ids.len()).unwrap_or(u32::MAX),
            },
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(*id) {
            return Err(LabeledOperationError::DuplicateBlockMinted(
                super::error::DuplicateBlockMinted { id: *id },
            ));
        }
    }
    Ok(())
}

/// Where a root array is stored, for both roots.
///
/// The edge root and the value root differ in exactly one thing - the store their array lives in - and differ in nothing
/// else: both are `entry_count` 4-byte block ids, both are written *after* the blocks they name, and both replace a
/// region or a slot range that is retired only after the descriptor is published. Modelling the location lets one
/// `write_root_array` serve both, so the ordering rules cannot be implemented twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootArrayStore {
    /// Edge slots in the leaf, `E::BYTES` per entry with the id in the first four bytes.
    LeafSlots { start: u64, slot_bytes: u32 },
    /// The same, held by an edge spill run: one entry per row.
    LeafRun {
        run: crate::labeled::spill_run_store::SpillRunId,
        rows: u32,
    },
    /// A byte region in the value store: `4` bytes per entry, packed.
    ValueBytes { offset: u64 },
}

impl<'a, E, M> BucketTreePlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Writes an id array into whichever store holds it, padding leaf slots to a whole record.
    ///
    /// The one write path for both roots: a caller states the location and the entries, and the store-specific byte
    /// layout lives here. Leaf slots pad each id to `slot_bytes` (a raw copy moves the array), the value store packs
    /// them at four bytes.
    pub(crate) fn write_root_array(
        &self,
        store: RootArrayStore,
        entries: &[u32],
    ) -> Result<(), LabeledOperationError> {
        match store {
            RootArrayStore::ValueBytes { offset } => {
                let mut bytes = Vec::with_capacity(entries.len() * 4);
                for id in entries {
                    bytes.extend_from_slice(&id.to_le_bytes());
                }
                self.graph
                    .values()
                    .write_bytes(offset, &bytes)
                    .map_err(|_| {
                        LabeledOperationError::from(
                        crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow,
                    )
                    })
            }
            RootArrayStore::LeafRun { run, rows } => {
                if entries.len() > rows as usize {
                    return Err(LabeledOperationError::RootEntryCountMismatch(
                        super::error::RootEntryCountMismatch {
                            root: "edge",
                            expected: rows,
                            got: u32::try_from(entries.len()).unwrap_or(u32::MAX),
                        },
                    ));
                }
                for (index, id) in entries.iter().enumerate() {
                    self.graph
                        .edges()
                        .spill_write_row(run, index as u32, &id.to_le_bytes())
                        .map_err(LabeledOperationError::from)?;
                }
                Ok(())
            }
            RootArrayStore::LeafSlots { start, slot_bytes } => {
                let slot_bytes = slot_bytes as usize;
                let mut image = vec![0u8; entries.len() * slot_bytes];
                for (index, id) in entries.iter().enumerate() {
                    let offset = index * slot_bytes;
                    image[offset..offset + 4].copy_from_slice(&id.to_le_bytes());
                }
                self.graph
                    .edges()
                    .write_slots_contiguous_bytes(start, &image)
                    .map_err(LabeledOperationError::from)
            }
        }
    }

    /// A root array's store, derived from the descriptor being written.
    pub(crate) fn edge_root_store(&self, edge_start: u64, slot_bytes: u32) -> RootArrayStore {
        match self.edge_root_loc() {
            EdgeRootLoc::Run { run, rows } => RootArrayStore::LeafRun { run, rows },
            EdgeRootLoc::Slots { .. } => RootArrayStore::LeafSlots {
                start: edge_start,
                slot_bytes,
            },
        }
    }

    /// The leaf-slot store for a given start, for tests that exercise the writer directly.
    #[cfg(test)]
    pub(crate) fn edge_root_store_for_test(start: u64, slot_bytes: u32) -> RootArrayStore {
        RootArrayStore::LeafSlots { start, slot_bytes }
    }

    /// The value root's store for a descriptor whose value-root region starts at `offset`.
    pub(crate) fn value_root_store(offset: u64) -> RootArrayStore {
        RootArrayStore::ValueBytes { offset }
    }
}

/// A value-root region **this attempt owns**.
///
/// The region exists between the allocation and the descriptor that records it, and the failure in between is the
/// interesting one: with ownership as a bare offset nobody holds it, so a failed attempt leaks the span and a retried
/// attempt allocates a second one over a region whose bytes stay behind. As a value with a drop, the owner is the
/// attempt itself - an early return releases the span, and only a successful publication disarms it.
///
/// The release is best-effort, as every release in this subsystem is: a failed release leaks a span, never data.
pub(crate) struct OwnedValueRoot<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
    offset: u64,
    bytes: u64,
    /// `true` until the descriptor that records this region has been published.
    armed: bool,
}

impl<'a, E, M> OwnedValueRoot<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The address a descriptor may record for this region.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// States that the descriptor recording this region is published: the drop no longer releases it.
    pub(crate) fn disarm(mut self) -> u64 {
        self.armed = false;
        self.offset
    }
}

impl<E, M> Drop for OwnedValueRoot<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .graph
                .values()
                .release_byte_span(self.offset, self.bytes);
        }
    }
}

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

    /// The value-root offset this plane's bucket owns, if it has a root to replace.
    ///
    /// The geometry answers whether a root exists (a bucket whose depth or width says it has none owns none), so a
    /// caller never has to read "is this offset real" out of the number itself.
    pub(crate) fn owned_value_root(&self) -> Option<u64> {
        (compact::value_root_region_bytes(&self.bucket) > 0)
            .then(|| self.bucket.inline_property_bytes_offset())
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
    /// Gives the value root a region in the value store and records the entries in it.
    ///
    /// `replaces` is the **ownership claim**: `Some(offset)` means this bucket owns the root at `offset` and releasing
    /// it is part of the operation; `None` means nothing is owned yet - a promotion builds the descriptor from a
    /// template whose offset is not an allocation, and passing it as if it were released a span the promotion never
    /// held (offset 0 is also a legitimate first span, so a sentinel cannot express this).
    /// Allocates the value root's region, writes the entries, and answers the **owned** region.
    pub(crate) fn acquire_value_root(
        &self,
        value_children: &[u32],
    ) -> Result<OwnedValueRoot<'_, E, M>, super::LabeledOperationError> {
        let children_len = value_children.len() * 4;
        let offset = self
            .graph
            .values()
            .allocate_byte_span(children_len as u64)
            .map_err(|_| {
                super::LabeledOperationError::from(
                    crate::lara::operation_error::LaraOperationError::overflow(),
                )
            })?;
        // The same writer as the edge root: only the store differs.
        self.write_root_array(RootArrayStore::ValueBytes { offset }, value_children)?;
        Ok(OwnedValueRoot {
            graph: self.graph,
            offset,
            bytes: children_len as u64,
            armed: true,
        })
    }

    /// Allocates the value root and records it in a descriptor the caller publishes immediately, releasing the root it
    /// replaces.
    ///
    /// `replaces` is the **ownership claim** for that release: `Some(offset)` means this bucket owns the root there and
    /// releasing it is part of the operation; `None` means nothing is owned yet. Offset 0 cannot express the
    /// difference, because it is also a legitimate first span.
    pub(crate) fn ensure_value_root(
        &self,
        value_children: &[u32],
        replaces: Option<u64>,
    ) -> Result<u64, super::LabeledOperationError> {
        let replaced = compact::value_root_region_bytes(&self.bucket);
        let root = self.acquire_value_root(value_children)?;
        let offset = root.disarm();
        if let Some(previous) = replaces
            && replaced > 0
            && previous != offset
        {
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
        // Both roots are id arrays; only the store differs, so the write goes through the one `write_root_array`.
        // An image that is not a whole number of slots is padded first: its trailing bytes belong to the last slot and
        // dropping them would leave that slot's tail as it was.
        let slot_bytes = E2::BYTES;
        let mut image = edge_root_image.to_vec();
        if slot_bytes > 0 && !image.len().is_multiple_of(slot_bytes) {
            image.resize(image.len().div_ceil(slot_bytes) * slot_bytes, 0);
        }
        let entries: Vec<u32> = image
            .chunks_exact(slot_bytes.max(1))
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        self.write_root_array(
            self.edge_root_store(edge_start, u32::try_from(slot_bytes).unwrap_or(u32::MAX)),
            &entries,
        )
    }

    /// Writes both roots and hands back the value root **this attempt owns**, for a caller that publishes the
    /// descriptor itself.
    ///
    /// It deliberately does **not** release the region it replaces. Releasing before the caller writes the descriptor
    /// that names the new one leaves the descriptor pointing at freed bytes between the two writes, and the value
    /// payloads then land on them - `[1, 2, 3, 1, 6]` at the root's address was that. The caller retires the old region
    /// after its publish, and the returned owner returns the new one if the publish never happens.
    pub(crate) fn write_roots<E2>(
        &self,
        edge_start: u64,
        edge_root_image: &[u8],
        value_children: &[u32],
    ) -> Result<Option<OwnedValueRoot<'_, E, M>>, super::LabeledOperationError>
    where
        E2: CsrEdge + crate::traits::CsrEdgeTombstone,
    {
        let owned = if value_children.is_empty() {
            None
        } else {
            Some(self.acquire_value_root(value_children)?)
        };
        self.write_edge_root::<E2>(edge_start, edge_root_image)?;
        Ok(owned)
    }

    /// The value-root region this bucket owns, for a caller that replaces it.
    pub(crate) fn owns_value_root(&self) -> Option<(u64, u32)> {
        let bytes = compact::value_root_region_bytes(&self.bucket);
        (bytes > 0).then(|| (self.bucket.inline_property_bytes_offset(), bytes))
    }

    /// Retires a value-root region this bucket owned, after the descriptor that named it is gone.
    pub(crate) fn retire_value_root(&self, offset: u64, bytes: u32) {
        let _ = self
            .graph
            .values()
            .release_byte_span(offset, u64::from(bytes));
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
        // **The replaced region is retired last.** Releasing it before the descriptor that names the new one is the
        // same ordering defect as writing a pointer before its payload: if the descriptor write fails, the old region is
        // already free while the descriptor still names it, so the next allocation writes into a live root and the tree
        // reads garbage ids (`[1, 2, 3, 1, 6]` was that). The owned region also returns itself if anything above fails.
        let replaced = self.owned_value_root();
        let replaced_bytes = compact::value_root_region_bytes(&self.bucket);
        let mut owned = if value_children.is_empty() {
            None
        } else {
            Some(self.acquire_value_root(value_children)?)
        };
        if let Some(root) = owned.as_ref() {
            published = published.with_inline_property_bytes_offset(root.offset());
        }
        if let Err(error) = self
            .graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, published)
        {
            // `owned` is still armed: it returns its region on drop, and the descriptor still names the old one.
            return Err(LabeledOperationError::from(error));
        }
        if let Some(root) = owned.take() {
            root.disarm();
        }
        if let Some(previous) = replaced
            && replaced_bytes > 0
            && Some(previous) != published.inline_property_bytes_offset().into()
        {
            let _ = self
                .graph
                .values()
                .release_byte_span(previous, u64::from(replaced_bytes));
        }
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
    /// Both roots are id arrays and both are written by one function; the store is the only difference.
    ///
    /// The value store packs entries at four bytes and the leaf slots pad each id to the record width, so one writer
    /// serving both proves the layouts really are the same array in two stores - the property the edge and value roots
    /// were implemented twice without stating.
    #[test]
    fn one_writer_serves_both_root_stores() {
        let graph = inline_property_test_graph();
        let bucket = install(&graph, &[11, 12, 13, 14], &[21, 22, 23]);
        let plane = graph.tree_plane(bucket);

        // Value store: packed at four bytes.
        let value_store =
            <BucketTreePlane<'_, InlinePropertyTestEdge, VectorMemory>>::value_root_store(4096);
        plane
            .write_root_array(value_store, &[7, 8, 9])
            .expect("write the value root");
        let mut packed = [0u8; 12];
        graph.values().read_bytes(4096, &mut packed);
        assert_eq!(
            packed
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect::<Vec<_>>(),
            vec![7, 8, 9]
        );

        // Leaf slots: each id padded to the record width, so a raw copy moves the array.
        let (start, _slots) = plane.edge_root();
        let slot_store =
            <BucketTreePlane<'_, InlinePropertyTestEdge, VectorMemory>>::edge_root_store_for_test(
                start, 8,
            );
        plane
            .write_root_array(slot_store, &[31, 32])
            .expect("write the edge root");
        let mut slots = [0u8; 16];
        graph.edges().read_slots_contiguous_bytes(start, &mut slots);
        assert_eq!(
            slots
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect::<Vec<_>>(),
            vec![31, 32]
        );
    }

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
            .ensure_value_root(&[21, 22, 23, 24, 25], Some(before))
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
