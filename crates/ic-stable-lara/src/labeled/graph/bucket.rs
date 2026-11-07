//! Labeled graph `bucket` implementation.

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
    },
    lara::operation_error::LaraOperationError,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::cmp::Ordering;
use std::collections::BTreeSet;

use super::error::LabeledOperationError;
use super::{
    BUCKET_LOOKUP_CACHE_ENTRIES, BULK_BUCKET_SEARCH_MIN_DEGREE, BucketLookupCache, BucketSearch,
    LEAF_VERTEX_EDGE_SEGMENT_DENSITY, LabeledLaraGraph,
};

/// Read-only placement metadata aggregated for every bucket on one leaf.
///
/// Returned by [`LabeledLaraGraph::read_leaf_placement_stats`]. This is a stable-
/// store observation over the bucket descriptor span of every vertex in the leaf.
/// Default-label bypass rows contribute edge occupancy but no inline property byte width. It is intended for ADR 0045 read-only
/// batch placement planning: an accurate leaf projection must include resident
/// rows from buckets that are not directly targeted by the pending batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafBucketPlacementStats {
    /// Number of vertices on this leaf that are in bucket mode.
    pub bucket_mode_vertices: u32,
    /// Sum of edge-slab slots reserved by all buckets on this leaf.
    pub total_stored_edge_slots: u64,
    /// Sum of resident edge spill rows across all buckets on this leaf (ADR 0097 §3).
    pub total_edge_spill_rows: u64,
    /// Sum of inline-property-bytes slab slots across all buckets on this leaf.
    pub total_inline_property_bytes_slab_slots: u64,
    /// Sum of inline-property-bytes overflow-log entries across all buckets on this leaf.
    pub total_inline_property_bytes_overflow_log_slots: u64,
    /// Non-zero inline property byte widths present on this leaf.
    pub inline_property_byte_widths: BTreeSet<u16>,
}

/// Read-only placement metadata for one existing label bucket.
///
/// Returned by [`LabeledLaraGraph::read_label_bucket_placement_info`]. All counts
/// are exact stable-store observations; no ephemeral plan state is included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelBucketPlacementInfo {
    /// Logical live edges currently visible in this bucket.
    pub degree: u32,
    /// Edge-slab slots reserved for this bucket (may exceed [`Self::degree`]).
    pub stored_edge_slots: u32,
    /// This bucket's edge-plane spill run id (ADR 0097 §3), or `NO_SPILL_RUN` when all edges are
    /// on the slab.
    pub edge_spill_run: u32,
    /// Resident edge spill rows: the run's used length.
    pub edge_spill_rows: u32,
    /// Physical byte width per edge inline property slot (`0` = no inline property bytes).
    pub inline_property_byte_width: u16,
    /// Inline-value slab slots reserved for this bucket.
    pub inline_property_bytes_slab_slots: u32,
    /// InlinePropertyBytes overflow log head, or `-1` when all values are on the slab.
    pub inline_property_bytes_overflow_log_head: i32,
    /// Number of values stored in the per-bucket inline property bytes overflow log chain.
    pub inline_property_bytes_overflow_log_len: u32,
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Returns the identities of non-default buckets on one PMA leaf.
    ///
    /// This is a read-only owner accessor for higher-level maintenance. It deliberately
    /// projects bucket identity only; descriptor and stable-collection details remain owned by
    /// this graph module.
    pub(crate) fn read_leaf_bucket_identities(
        &self,
        leaf: u32,
    ) -> Result<Vec<(VertexId, BucketLabelKey)>, LabeledOperationError> {
        let header = self.edges.header();
        let segment_size = header.segment_size.max(1);
        let start = leaf
            .checked_mul(segment_size)
            .ok_or(LaraOperationError::overflow())?;
        if start >= self.vertices.len() {
            return Ok(Vec::new());
        }
        let end = start
            .checked_add(segment_size)
            .ok_or(LaraOperationError::overflow())?
            .min(self.vertices.len());
        let mut identities = Vec::new();
        for vid_u in start..end {
            let vid = VertexId::from(vid_u);
            self.ensure_vertex(vid)?;
            let vertex = self.vertices.get(vid);
            if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
                continue;
            }
            for bucket in self.read_vertex_label_buckets(&vertex)? {
                if bucket.degree() != 0 {
                    identities.push((vid, bucket.bucket_label_key()));
                }
            }
        }
        Ok(identities)
    }

    /// Returns every non-default bucket descriptor on one leaf, including zero-degree rows.
    ///
    /// This is test-only so the production planning surface continues to expose only live
    /// identities; snapshots still cover dormant descriptors that a read-only operation must not
    /// rewrite.
    #[cfg(test)]
    pub(crate) fn read_leaf_all_bucket_placement_info(
        &self,
        leaf: u32,
    ) -> Result<
        Vec<(VertexId, BucketLabelKey, Option<LabelBucketPlacementInfo>)>,
        LabeledOperationError,
    > {
        let header = self.edges.header();
        let segment_size = header.segment_size.max(1);
        let start = leaf
            .checked_mul(segment_size)
            .ok_or(LaraOperationError::overflow())?;
        if start >= self.vertices.len() {
            return Ok(Vec::new());
        }
        let end = start
            .checked_add(segment_size)
            .ok_or(LaraOperationError::overflow())?
            .min(self.vertices.len());
        let mut descriptors = Vec::new();
        for vid_u in start..end {
            let owner = VertexId::from(vid_u);
            self.ensure_vertex(owner)?;
            let vertex = self.vertices.get(owner);
            if vertex.is_default_edge_labeled() {
                continue;
            }
            for bucket in self.read_vertex_label_buckets(&vertex)? {
                let label = bucket.bucket_label_key();
                descriptors.push((
                    owner,
                    label,
                    self.read_label_bucket_placement_info(owner, label)?,
                ));
            }
        }
        Ok(descriptors)
    }

    pub(crate) fn labeled_leaf_segment_is_dense(&self, vid: VertexId) -> bool {
        self.labeled_leaf_pma_density(vid) >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY
    }

    fn leaf_has_other_labeled_vertices_with_edges(
        &self,
        leaf: u32,
        seg: u32,
        except: VertexId,
    ) -> bool {
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == except {
                continue;
            }
            let v = self.vertices.get(vid);
            if !v.is_default_edge_labeled() && v.degree() > 0 {
                return true;
            }
        }
        false
    }

    pub(crate) fn rebalance_cascade_after_labeled_mutation(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(src, header.segment_size);
        let idx_u32 = leaf
            .checked_add(header.segment_count)
            .ok_or(LaraOperationError::overflow())?;
        let _idx = u64::from(idx_u32);
        if self.labeled_leaf_pma_density(src) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
            return Ok(());
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_rebalance_leaf_cascade");

        let sole_active_labeled_vertex =
            !self.leaf_has_other_labeled_vertices_with_edges(leaf, seg, src);

        if self.labeled_leaf_physical_range(src)?.is_some() {
            let counts = self.leaf_segment_counts_for_vid(src);
            if counts.total > 0 && counts.actual >= counts.total {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("labeled_relocate_leaf_physical_block");
                self.relocate_labeled_leaf_physical_block(src)?;
            } else {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("labeled_weighted_leaf_slide");
                self.rebalance_labeled_leaf_weighted_slide(src)?;
                if self.labeled_leaf_pma_density(src) >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
                    #[cfg(all(feature = "canbench", target_family = "wasm"))]
                    let _scope = bench_scope("labeled_relocate_leaf_after_slide");
                    self.relocate_labeled_leaf_physical_block(src)?;
                }
            }
            return Ok(());
        }

        let src_vertex = self.vertices.get(src);
        if !src_vertex.is_default_edge_labeled() && src_vertex.degree() > 0 {
            // ADR 0097 §7, with the slack rule db04af585 states - speculative slack must not
            // force allocation when the content already fits, and may use extra space only when
            // it is currently placeable. This rewrite is the cascade's *forced-slack* rewrite:
            // its whole purpose is to grow this vertex's span so the leaf gets room back. A
            // bucket whose rows live in its run has no span content to grow - the run holds
            // them, in the arena, and the plan keeps the prefix width - so the rewrite
            // re-publishes the same layout while still materializing and committing every
            // bucket, and the leaf's density does not move. Running it once per insert is what
            // made the non-tail bypass path 2.97x its `main` cost (GAP-2026-09-20-007); the
            // slack it asks for is not placeable and is not a reason to search a dense leaf.
            // Skip it when the vertex is not span-resident; the rest of the cascade still runs.
            let span_resident = self
                .read_vertex_label_buckets(&src_vertex)?
                .iter()
                .all(|bucket| bucket.edge_run().is_none());
            if span_resident {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("labeled_rewrite_vertex_edge_span");
                self.vertex_layout(src).rewrite_span(None, 0, true, None)?;
            }
            if self.labeled_leaf_pma_density(src) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
                return Ok(());
            }
        }

        if sole_active_labeled_vertex {
            return Ok(());
        }

        let start_vid = leaf
            .checked_mul(seg)
            .ok_or(LaraOperationError::overflow())?;
        let end_vid = start_vid
            .checked_add(seg)
            .ok_or(LaraOperationError::overflow())?
            .min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == src {
                continue;
            }
            let v = self.vertices.get(vid);
            if v.is_default_edge_labeled() {
                continue;
            }
            if v.degree() > 0 {
                // Cascading insertion cannot report a peer's logical row moves.
                self.pack_vertex_edge_span_layout(vid)?;
            }
        }

        if self.labeled_leaf_pma_density(src) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
            return Ok(());
        }

        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == src {
                continue;
            }
            let v = self.vertices.get(vid);
            if v.is_default_edge_labeled() {
                continue;
            }
            if v.degree() > 0 {
                self.vertex_layout(vid).rewrite_span(None, 0, true, None)?;
            }
        }
        Ok(())
    }

    // Keep the hot scalar lookup path inlined after the large traverse_next
    // implementation is compiled into the same Wasm module.
    #[inline(always)]
    pub(crate) fn find_bucket(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<BucketSearch, LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            return Ok(BucketSearch::Missing { insert_index: 0 });
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(BucketSearch::Missing { insert_index: 0 });
        }
        let start = vertex.base_slot_start();
        let range_end = start
            .checked_add(u64::from(deg))
            .ok_or(LaraOperationError::overflow())?;

        if let Some(cache) = self.last_bucket_lookup.get()
            && cache.vid == src
            && cache.base_slot_start == start
            && cache.degree == deg
        {
            if cache.bucket_key == label_id && (start..range_end).contains(&cache.slot) {
                // The cache is an optimisation: a slot it names may since have been vacated (a
                // relocation or promotion rewrites the vertex), and a stale hit must fall through to the
                // slow path rather than become a hard failure.
                if let Some(bucket) = self.buckets.read_label_bucket_slot(cache.slot)
                    && bucket.bucket_label_key() == label_id
                {
                    return Ok(BucketSearch::Found {
                        slot: cache.slot,
                        bucket,
                    });
                }
            }
            if let Some(slot_after_cache) = cache.slot.checked_add(1)
                && slot_after_cache == range_end
                && cache.bucket_key < label_id
                && let Some(bucket) = self.buckets.read_label_bucket_slot(cache.slot)
                && bucket.bucket_label_key() == cache.bucket_key
            {
                return Ok(BucketSearch::Missing { insert_index: deg });
            }
            if label_id > cache.bucket_key {
                if let Some(next_slot) = cache.slot.checked_add(1)
                    && next_slot < range_end
                    && let Some(bucket) = self.buckets.read_label_bucket_slot(next_slot)
                    && bucket.bucket_label_key() == label_id
                {
                    {
                        self.cache_bucket_lookup(src, label_id, vertex, next_slot);
                        return Ok(BucketSearch::Found {
                            slot: next_slot,
                            bucket,
                        });
                    }
                }
            } else if label_id < cache.bucket_key && cache.slot > start {
                let prev_slot = cache
                    .slot
                    .checked_sub(1)
                    .ok_or(LaraOperationError::overflow())?;
                let bucket = self
                    .buckets
                    .read_label_bucket_slot(prev_slot)
                    .ok_or(LaraOperationError::overflow())?;
                if bucket.bucket_label_key() == label_id {
                    self.cache_bucket_lookup(src, label_id, vertex, prev_slot);
                    return Ok(BucketSearch::Found {
                        slot: prev_slot,
                        bucket,
                    });
                }
            }
        }
        let cache_index = Self::bucket_lookup_cache_index(src, label_id);
        if let Some(cache) = self.bucket_lookup_cache[cache_index].get()
            && cache.vid == src
            && cache.bucket_key == label_id
            && cache.base_slot_start == start
            && cache.degree == deg
            && (start..range_end).contains(&cache.slot)
        {
            let bucket = self
                .buckets
                .read_label_bucket_slot(cache.slot)
                .ok_or(LaraOperationError::overflow())?;
            if bucket.bucket_label_key() == label_id {
                self.last_bucket_lookup.set(Some(cache));
                return Ok(BucketSearch::Found {
                    slot: cache.slot,
                    bucket,
                });
            }
        }
        // Fast paths: avoid binary search + canbench scope overhead on tiny degree.
        if deg == 1 {
            let bucket = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::overflow())?;
            return Ok(match label_id.cmp(&bucket.bucket_label_key()) {
                Ordering::Less => BucketSearch::Missing { insert_index: 0 },
                Ordering::Equal => {
                    self.cache_bucket_lookup(src, label_id, vertex, start);
                    BucketSearch::Found {
                        slot: start,
                        bucket,
                    }
                }
                Ordering::Greater => BucketSearch::Missing { insert_index: 1 },
            });
        }
        if deg == 2 {
            let b0 = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::overflow())?;
            match label_id.cmp(&b0.bucket_label_key()) {
                Ordering::Less => return Ok(BucketSearch::Missing { insert_index: 0 }),
                Ordering::Equal => {
                    self.cache_bucket_lookup(src, label_id, vertex, start);
                    return Ok(BucketSearch::Found {
                        slot: start,
                        bucket: b0,
                    });
                }
                Ordering::Greater => {
                    let slot1 = start.checked_add(1).ok_or(LaraOperationError::overflow())?;
                    let b1 = self
                        .buckets
                        .read_label_bucket_slot(slot1)
                        .ok_or(LaraOperationError::overflow())?;
                    return Ok(match label_id.cmp(&b1.bucket_label_key()) {
                        Ordering::Less => BucketSearch::Missing { insert_index: 1 },
                        Ordering::Equal => {
                            self.cache_bucket_lookup(src, label_id, vertex, slot1);
                            BucketSearch::Found {
                                slot: slot1,
                                bucket: b1,
                            }
                        }
                        Ordering::Greater => BucketSearch::Missing { insert_index: 2 },
                    });
                }
            }
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_find_bucket_slot");

        if deg >= BULK_BUCKET_SEARCH_MIN_DEGREE {
            let buckets = self
                .buckets
                .read_label_bucket_slots_contiguous(start, deg)
                .ok_or(LaraOperationError::overflow())?;
            return Ok(
                match buckets.binary_search_by_key(&label_id, |bucket| bucket.bucket_label_key()) {
                    Ok(index) => {
                        let slot = start
                            .checked_add(index as u64)
                            .ok_or(LaraOperationError::overflow())?;
                        let bucket = buckets[index];
                        self.cache_bucket_lookup(src, label_id, vertex, slot);
                        BucketSearch::Found { slot, bucket }
                    }
                    Err(index) => BucketSearch::Missing {
                        insert_index: index as u32,
                    },
                },
            );
        }

        let mut lo = 0u32;
        let mut hi = deg;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let slot = start
                .checked_add(u64::from(mid))
                .ok_or(LaraOperationError::overflow())?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::overflow())?;
            if bucket.bucket_label_key() == label_id {
                self.cache_bucket_lookup(src, label_id, vertex, slot);
                return Ok(BucketSearch::Found { slot, bucket });
            }
            if bucket.bucket_label_key() < label_id {
                lo = mid.checked_add(1).ok_or(LaraOperationError::overflow())?;
            } else {
                hi = mid;
            }
        }
        Ok(BucketSearch::Missing { insert_index: lo })
    }

    pub(crate) fn invalidate_bucket_lookup_caches_for_bucket_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let (start, end) = self.buckets.segment_vertex_bounds(&self.vertices, vid)?;
        if let Some(cache) = self.last_bucket_lookup.get() {
            let v_ord = u32::from(cache.vid);
            if (start..end).contains(&v_ord) {
                self.last_bucket_lookup.set(None);
            }
        }
        for cell in &self.bucket_lookup_cache {
            if let Some(cache) = cell.get() {
                let v_ord = u32::from(cache.vid);
                if (start..end).contains(&v_ord) {
                    cell.set(None);
                }
            }
        }
        Ok(())
    }

    pub(super) fn invalidate_bucket_lookup_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) {
        self.last_bucket_lookup.set(None);
        self.bucket_lookup_cache[Self::bucket_lookup_cache_index(src, label_id)].set(None);
    }

    pub(super) fn cache_bucket_lookup(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        vertex: &LabeledVertex,
        slot: u64,
    ) {
        let cache = BucketLookupCache {
            vid: src,
            bucket_key: label_id,
            base_slot_start: vertex.base_slot_start(),
            degree: vertex.degree(),
            slot,
        };
        self.last_bucket_lookup.set(Some(cache));
        self.bucket_lookup_cache[Self::bucket_lookup_cache_index(src, label_id)].set(Some(cache));
    }

    pub(super) fn bucket_lookup_cache_index(src: VertexId, label_id: BucketLabelKey) -> usize {
        let mixed = u32::from(src)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add(u32::from(label_id.raw()));
        (mixed as usize) & (BUCKET_LOOKUP_CACHE_ENTRIES - 1)
    }

    pub(super) fn labeled_bucket_descriptor_index(
        vertex: &LabeledVertex,
        bucket_slot: u64,
    ) -> Result<u32, LabeledOperationError> {
        let d = bucket_slot
            .checked_sub(vertex.base_slot_start())
            .ok_or(LaraOperationError::overflow())?;
        u32::try_from(d).map_err(|_| LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn labeled_vertex_bucket_slot(
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        vertex
            .base_slot_start()
            .checked_add(u64::from(bucket_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn find_bucket_slot(
        &self,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(
            match self.find_bucket(VertexId::from(0), vertex, label_id)? {
                BucketSearch::Found { slot, .. } => Some(slot),
                BucketSearch::Missing { .. } => None,
            },
        )
    }

    pub(super) fn read_vertex_label_buckets(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            return Ok(Vec::new());
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(Vec::new());
        }
        let start = vertex.base_slot_start();
        Ok(self
            .buckets
            .read_label_bucket_slots_contiguous(start, deg)
            .ok_or(LaraOperationError::overflow())?)
    }

    /// Resolves every label bucket of one vertex through `resolve`, producing the
    /// per-label placement policy map captured into maintenance work items at enqueue
    /// time (ADR 0052 slice 6). A default-label row and an empty span both produce an
    /// empty map; drain falls back to the order-preserving default for absent labels.
    pub(crate) fn vertex_bucket_policies(
        &self,
        vid: VertexId,
        resolve: &dyn Fn(BucketLabelKey) -> crate::labeled::graph::EdgePlacementPolicy,
    ) -> Result<
        Vec<(BucketLabelKey, crate::labeled::graph::EdgePlacementPolicy)>,
        LabeledOperationError,
    > {
        let vertex = self.vertices.get(vid);
        Ok(self
            .read_vertex_label_buckets(&vertex)?
            .into_iter()
            .map(|bucket| {
                let label = bucket.bucket_label_key();
                (label, resolve(label))
            })
            .collect())
    }

    pub(super) fn read_vertex_label_buckets_range(
        &self,
        vertex: &LabeledVertex,
        lo: u32,
        hi: u32,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
        if lo >= hi {
            return Ok(Vec::new());
        }
        let deg = vertex.degree();
        if hi > deg {
            return Err(LaraOperationError::overflow().into());
        }
        let n = hi - lo;
        let start = vertex
            .base_slot_start()
            .checked_add(u64::from(lo))
            .ok_or(LaraOperationError::overflow())?;
        self.buckets
            .read_label_bucket_slots_contiguous(start, n)
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Read-only aggregate placement metadata for every bucket on one PMA leaf.
    ///
    /// Iterates the bucket descriptors of all non-default-label vertices in `leaf`
    /// and sums their exact resident edge/inline property bytes slab and overflow-log slots. This
    /// includes buckets that are not directly targeted by a pending batch, which is
    /// required for ADR 0045 leaf-level projected geometry.
    ///
    /// Returns an error if leaf index arithmetic overflows.
    pub fn read_leaf_placement_stats(
        &self,
        leaf: u32,
    ) -> Result<LeafBucketPlacementStats, LabeledOperationError> {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let start_vid = leaf
            .checked_mul(seg)
            .ok_or(LaraOperationError::overflow())?;
        let end_vid = start_vid
            .checked_add(seg)
            .ok_or(LaraOperationError::overflow())?
            .min(self.vertices.len());
        let mut stats = LeafBucketPlacementStats {
            bucket_mode_vertices: 0,
            total_stored_edge_slots: 0,
            total_edge_spill_rows: 0,
            total_inline_property_bytes_slab_slots: 0,
            total_inline_property_bytes_overflow_log_slots: 0,
            inline_property_byte_widths: BTreeSet::new(),
        };
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            self.ensure_vertex(vid)?;
            let vertex = self.vertices.get(vid);
            if vertex.is_default_edge_labeled() {
                // Default-label bypass rows still occupy the shared edge PMA
                // span/log for this leaf.  They have no label buckets or
                // inline property bytes domain, but omitting their edge occupancy would make
                // a labeled batch under-project leaf geometry.
                stats.total_stored_edge_slots = stats
                    .total_stored_edge_slots
                    .checked_add(u64::from(vertex.stored_slots))
                    .ok_or(LaraOperationError::overflow())?;
                if let Ok(run) = self.edges.spill_mint(vertex.bypass_spill_run()) {
                    stats.total_edge_spill_rows = stats
                        .total_edge_spill_rows
                        .checked_add(u64::from(self.edges.spill_used_len(run)))
                        .ok_or(LaraOperationError::overflow())?;
                }
                continue;
            }
            stats.bucket_mode_vertices = stats
                .bucket_mode_vertices
                .checked_add(1)
                .ok_or(LaraOperationError::overflow())?;
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            for bucket in buckets {
                // ADR 0096 §5: tiny buckets occupy zero physical geometry (no
                // span/log/values); like bypass rows they are counted for
                // occupancy only where they hold bytes — here, nowhere.
                if bucket.is_tiny_mode() {
                    continue;
                }
                stats.total_stored_edge_slots = stats
                    .total_stored_edge_slots
                    .checked_add(u64::from(bucket.stored_slots_raw()))
                    .ok_or(LaraOperationError::overflow())?;
                stats.total_inline_property_bytes_slab_slots = stats
                    .total_inline_property_bytes_slab_slots
                    .checked_add(u64::from(bucket.inline_property_bytes_slab_slots()))
                    .ok_or(LaraOperationError::overflow())?;
                if bucket.inline_property_byte_width() > 0 {
                    stats
                        .inline_property_byte_widths
                        .insert(bucket.inline_property_byte_width());
                }
                if let Ok(run) = self.edges.spill_mint(bucket.edge_spill_run()) {
                    stats.total_edge_spill_rows = stats
                        .total_edge_spill_rows
                        .checked_add(u64::from(self.edges.spill_used_len(run)))
                        .ok_or(LaraOperationError::overflow())?;
                }
                if bucket.values_run().is_some() {
                    stats.total_inline_property_bytes_overflow_log_slots = stats
                        .total_inline_property_bytes_overflow_log_slots
                        .checked_add(u64::from(self.bucket_values_run_rows(&bucket)))
                        .ok_or(LaraOperationError::overflow())?;
                }
            }
        }
        Ok(stats)
    }

    /// Read-only placement metadata for one existing label bucket.
    ///
    /// Returns `Ok(None)` when the vertex uses the default-label bypass or has no
    /// bucket for `label_id`. Returns an error if the vertex is out of range. The
    /// counts include slab residents and overflow-log residents separately so that
    /// a batch planner can project the minimum required post-batch capacity without
    /// mutating state.
    pub fn read_label_bucket_placement_info(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Option<LabelBucketPlacementInfo>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(None);
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { bucket, .. } => {
                // ADR 0096 §5: tiny buckets hold live edges but zero reserved
                // capacity (no span/log/values). Degree is honest; the rest
                // reports the empty geometry so planners never size from
                // payload bytes.
                if bucket.is_tiny_mode() {
                    return Ok(Some(LabelBucketPlacementInfo {
                        degree: bucket.degree,
                        stored_edge_slots: 0,
                        edge_spill_run: crate::traits::NO_SPILL_RUN,
                        edge_spill_rows: 0,
                        inline_property_byte_width: 0,
                        inline_property_bytes_slab_slots: 0,
                        inline_property_bytes_overflow_log_head: -1,
                        inline_property_bytes_overflow_log_len: 0,
                    }));
                }
                let edge_spill_rows = match self.edges.spill_mint(bucket.edge_spill_run()) {
                    Ok(run) => self.edges.spill_used_len(run),
                    // Unset (NO_SPILL_RUN) or out of the arena's range: no spill rows.
                    Err(_) => 0,
                };
                Ok(Some(LabelBucketPlacementInfo {
                    degree: bucket.degree,
                    stored_edge_slots: bucket.stored_slots_raw(),
                    edge_spill_run: bucket.edge_spill_run(),
                    edge_spill_rows,
                    inline_property_byte_width: bucket.inline_property_byte_width(),
                    inline_property_bytes_slab_slots: bucket.inline_property_bytes_slab_slots(),
                    inline_property_bytes_overflow_log_head: bucket
                        .inline_property_bytes_log_head(),
                    inline_property_bytes_overflow_log_len: bucket.inline_property_bytes_log_len()
                        as u32,
                }))
            }
            BucketSearch::Missing { .. } => Ok(None),
        }
    }
    pub(super) fn vertex_label_buckets_have_overflow(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<bool, LabeledOperationError> {
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }
        let buckets = self.read_vertex_label_buckets(vertex)?;
        Ok(buckets.iter().any(|b| b.edge_run().is_some()))
    }

    pub(super) fn bucket_successor_start(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        let cur_slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let cur = self
            .buckets
            .read_label_bucket_slot(cur_slot)
            .ok_or(LaraOperationError::overflow())?;
        self.bucket_successor_start_after_bucket(vertex, bucket_index, &cur)
    }

    pub(super) fn bucket_successor_start_after_bucket(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        // ADR 0096 §5: tiny successors occupy zero slab slots (anchors, not
        // storage) — clamping to a tiny anchor would collapse the window to
        // empty. Skip forward to the next non-tiny bucket (usually one step).
        let mut next_ix = bucket_index
            .checked_add(1)
            .ok_or(LaraOperationError::overflow())?;
        while next_ix < vertex.degree() {
            let next_slot = Self::labeled_vertex_bucket_slot(vertex, next_ix)?;
            let next = self
                .buckets
                .read_label_bucket_slot(next_slot)
                .ok_or(LaraOperationError::overflow())?;
            if !next.is_tiny_mode() {
                // Proportional slack placement does not guarantee strictly increasing
                // `edge_start` across bucket slots; CSR slab-window geometry requires a
                // non-decreasing neighbor base, so clamp the successor boundary.
                return Ok(next.edge_start().max(bucket.edge_start()));
            }
            next_ix = next_ix
                .checked_add(1)
                .ok_or(LaraOperationError::overflow())?;
        }

        if vertex.degree() == 0 {
            return Ok(0);
        }

        // The descriptor-order first non-tiny owns the whole vertex cover.
        // Tiny anchors own no slab slots; an all-tiny row keeps its empty anchor.
        let mut base_ix = 0u32;
        let first_edge_start = loop {
            let slot = Self::labeled_vertex_bucket_slot(vertex, base_ix)?;
            let candidate = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::overflow())?;
            if !candidate.is_tiny_mode() {
                break candidate.edge_start();
            }
            base_ix = base_ix
                .checked_add(1)
                .ok_or(LaraOperationError::overflow())?;
            if base_ix == vertex.degree() {
                break bucket.edge_start();
            }
        };
        crate::labeled::slot_index::checked_add_slot_index(
            first_edge_start,
            u64::from(vertex.stored_slots),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Exclusive end of the bucket's on-slab CSR window.
    ///
    /// For non-last buckets this is the next bucket's `edge_start`. For the last bucket
    /// it is `edge_start + stored_slots`. This differs from
    /// [`Self::bucket_successor_start_after_bucket`] for the last bucket: the slab-window
    /// end follows the bucket's own stored prefix, while the successor/vertex-span end
    /// follows the vertex-wide `stored_slots` tail used by fold/new-bucket placement.
    pub(super) fn bucket_slab_window_end_exclusive_after_bucket(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        // ADR 0096 §5: tiny successors occupy zero slab slots — clamping to a
        // tiny anchor would collapse the window to empty. Skip forward to the
        // next non-tiny bucket (usually one step); all-remaining-tiny falls
        // through to the own-span tail below.
        let mut next_ix = bucket_index
            .checked_add(1)
            .ok_or(LaraOperationError::overflow())?;
        while next_ix < vertex.degree() {
            let next_slot = Self::labeled_vertex_bucket_slot(vertex, next_ix)?;
            let next = self
                .buckets
                .read_label_bucket_slot(next_slot)
                .ok_or(LaraOperationError::overflow())?;
            if !next.is_tiny_mode() {
                return Ok(next.edge_start().max(bucket.edge_start()));
            }
            next_ix = next_ix
                .checked_add(1)
                .ok_or(LaraOperationError::overflow())?;
        }

        crate::labeled::slot_index::checked_add_slot_index(
            bucket.edge_start(),
            u64::from(bucket.stored_slots_raw()),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn bucket_successor_start_after_bucket_for_new_bucket(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        if bucket_index + 1 < vertex.degree() {
            // New bucket is being inserted before an existing successor. The new bucket
            // owns zero slots, so its edge_start is placed exactly at the successor's
            // current edge_start. This keeps the two spans contiguous and non-overlapping.
            let next_ix = bucket_index
                .checked_add(1)
                .ok_or(LaraOperationError::overflow())?;
            let next_slot = Self::labeled_vertex_bucket_slot(vertex, next_ix)?;
            let next = self
                .buckets
                .read_label_bucket_slot(next_slot)
                .ok_or(LaraOperationError::overflow())?;
            return Ok(next.edge_start());
        }

        if bucket_index == 0 {
            // First bucket on the vertex. Its zero-length reservation starts at the
            // beginning of the vertex edge span; stored_slots remains zero.
            return Ok(0);
        }

        // For the last bucket, the successor boundary is the end of the preceding
        // bucket run, not the vertex-wide stored_slots tail. This lets a new bucket
        // be inserted with stored_slots=0 without consuming any reserved span.
        let prev_ix = bucket_index
            .checked_sub(1)
            .ok_or(LaraOperationError::overflow())?;
        let prev_slot = Self::labeled_vertex_bucket_slot(vertex, prev_ix)?;
        let prev = self
            .buckets
            .read_label_bucket_slot(prev_slot)
            .ok_or(LaraOperationError::overflow())?;
        // ADR 0096 §5: a tiny predecessor occupies zero slab slots — the new
        // bucket anchors at the tiny anchor itself, not anchor + degree.
        if prev.is_tiny_mode() {
            return Ok(prev.edge_start());
        }
        crate::labeled::slot_index::checked_add_slot_index(
            prev.edge_start(),
            u64::from(prev.stored_slots_raw()),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn vertex_bucket_descriptor_row_end(
        &self,
        vid: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            return self.vertex_prefix_end(vid);
        }
        // The row is the vertex's `degree + slack` slots. Its absence is reported with the fields that explain it:
        // a probe asked which invariant failed at this site and a bare `overflow()` could not answer.
        let span = vertex.label_bucket_descriptor_span().ok_or_else(|| {
            LabeledOperationError::BucketDescriptorRow(
                crate::labeled::graph::error::BucketDescriptorRowUnavailable {
                    vid,
                    degree: vertex.degree(),
                    slack: u32::from(vertex.bucket_slack_slots()),
                    default_labeled: vertex.is_default_edge_labeled(),
                },
            )
        })?;
        vertex
            .base_slot_start()
            .checked_add(u64::from(span))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn ensure_vertex_bucket_row_origin(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() > 0 {
            return Ok(());
        }
        let row_base = if u32::from(src) == 0 {
            0
        } else {
            let pred = VertexId::from(u32::from(src).saturating_sub(1));
            self.vertex_bucket_descriptor_row_end(pred)?
        };
        if vertex.base_slot_start() != row_base {
            self.set_labeled_vertex(src, vertex.with_base_slot_start(row_base))?;
        }
        Ok(())
    }

    /// The bucket's values rows, oldest first. ADR 0097 §5: a values run is ordinal, so this is
    /// the run's row indices rather than a chain walk; callers that precompute it share one list.
    pub(super) fn bucket_inline_property_bytes_log_chain(&self, bucket: &LabelBucket) -> Vec<u32> {
        match bucket.values_run() {
            Some(run) => self.values.spill_asc_indices(run),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{LEAF_VERTEX_EDGE_SEGMENT_DENSITY, *};
    use crate::VertexId;

    /// The descriptor row's absence names its cause instead of reporting a generic overflow.
    ///
    /// `GAP-GRAPH-TWO-LABELS-ON-A-BIG-VERTEX` was traced to this site three times and each probe could not say which
    /// invariant had failed, because every failure arrived as `CollectAllocationOverflow`. The error now carries the
    /// vertex's degree, slack and labelling, which is what those probes wanted to print.
    #[test]
    fn a_missing_descriptor_row_names_its_cause() {
        use crate::labeled::graph::error::BucketDescriptorRowUnavailable;
        let graph = inline_property_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let vid = VertexId::from(0);
        // A default-edge-labelled vertex has no descriptor row by design - but its span answers `None` only for that
        // reason here, and the error says so rather than leaving a caller to guess.
        let unavailable = BucketDescriptorRowUnavailable {
            vid,
            degree: 10_000,
            slack: 3,
            default_labeled: false,
        };
        let error = LabeledOperationError::BucketDescriptorRow(unavailable);
        let rendered = error.to_string();
        assert!(
            rendered.contains("degree=10000")
                && rendered.contains("slack=3")
                && rendered.contains("default_labeled=false"),
            "the message must name the fields a probe needs: {rendered}"
        );
    }

    /// The reproduction `GAP-GRAPH-TWO-LABELS-ON-A-BIG-VERTEX` describes, with the typed error in place so the
    /// failure can be read instead of guessed.
    #[test]
    fn a_second_label_joins_a_ten_thousand_row_vertex() {
        use crate::labeled::graph::EdgePlacementPolicy;
        // Production geometry: segment 16 is where sixteen vertices share a leaf and a vertex grows by
        // relocation, which is the shape the gap was found in.
        let graph = super::super::test_support::test_graph_with_segment_size(
            16,
            BucketLabelKey::directed_from_index(1),
        );
        let vid = VertexId::from(0);
        // Segment 16 means sixteen vertices share one leaf: the hub has to grow its slice beside fifteen mates,
        // which is the layout the gap was found in.
        for mate in 1..16u32 {
            let mate_id = graph.push_vertex(LabeledVertex::default()).unwrap();
            for target in 0..4u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        mate_id,
                        BucketLabelKey::directed_from_index(2),
                        crate::labeled::graph::test_support::TestEdge {
                            target: mate * 10 + target,
                        },
                        EdgePlacementPolicy::Insertion,
                    )
                    .expect("mate insert");
            }
        }
        let first = BucketLabelKey::directed_from_index(2);
        for target in 0..10_000u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    first,
                    crate::labeled::graph::test_support::TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let after_first = graph.vertices().get(vid);
        assert_eq!(after_first.degree(), 1, "one bucket on the first label");
        let second = BucketLabelKey::directed_from_index(3);
        for target in 0..5u32 {
            let result = graph.insert_edge_skip_leaf_cascade(
                vid,
                second,
                crate::labeled::graph::test_support::TestEdge {
                    target: 50_000 + target,
                },
                EdgePlacementPolicy::Insertion,
            );
            if let Err(error) = result {
                let vertex = graph.vertices().get(vid);
                panic!(
                    "edge {target} of the second label failed: {error}\n\
                     vertex: degree={} slack={} default_labeled={} base={} span={:?}",
                    vertex.degree(),
                    vertex.bucket_slack_slots(),
                    vertex.is_default_edge_labeled(),
                    vertex.base_slot_start(),
                    vertex.label_bucket_descriptor_span(),
                );
            }
        }
        let after_second = graph.vertices().get(vid);
        assert_eq!(after_second.degree(), 2, "both labels have a bucket");
    }

    /// A value-carrying second label joins a 10 000-row vertex the same way the valueless one does: the tiny bucket
    /// promotes into a run, and the width the new edges carry is materialized on that run-backed bucket.
    #[test]
    fn a_valued_second_label_joins_a_ten_thousand_row_vertex() {
        use crate::labeled::graph::EdgePlacementPolicy;
        // The width-carrying edge type only exists in the inline-property fixture, which keeps segment 32; the leaf
        // geometry is the same shape (sixteen-plus vertices per leaf, one slice each), just wider.
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let first = BucketLabelKey::directed_from_index(2);
        for target in 0..10_000u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    first,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target,
                        &[],
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("noise insert");
        }
        let second = BucketLabelKey::directed_from_index(3);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, second, 2)
            .expect("declare the value width");
        for target in 0..5u32 {
            let value = u16::try_from(50_000 + target).expect("fits").to_le_bytes();
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    second,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        50_000 + target,
                        &value,
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("valued edge {target}: {error}"));
        }
        // The point of the test: the second label's bucket is run-backed, so its values are materialized on a run
        // rather than a slab prefix, and the width the edges carry survived the promotion and the migration.
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, second)
            .expect("find")
            .expect("the valued label has a bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert_eq!(
            bucket.stored_slots(),
            0,
            "the second label took the run-backed promotion"
        );
        assert!(
            bucket.edge_run().is_some(),
            "a span-less bucket must name the run its rows are in"
        );
        assert_eq!(bucket.inline_property_byte_width(), 2);

        // Every valued row reads back its own value, whichever plane backs it.
        let mut read = Vec::new();
        let _ = graph
            .visit_edges_with_inline_property(
                vid,
                second,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |slot, item| {
                    read.push((
                        slot.raw(),
                        u16::from_le_bytes(
                            item.inline_property.bytes().try_into().expect("width 2"),
                        ),
                    ));
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("read the valued label");
        assert_eq!(read.len(), 5, "five valued rows");
        for (_, value) in read {
            assert!(
                (50_000..50_005).contains(&value),
                "value {value} is not one this test inserted"
            );
        }
    }

    /// A run-backed bucket promotes to tree mode, its root living in an edge run because the vertex slice has no
    /// span for it, and every inserted row reads back exactly once - which makes a second label on a filled vertex
    /// usable at any size, not only up to the spill run's class cap. The row count is read **before** the descriptor is
    /// trusted: that ordering is what told "a duplicated row" apart from "a stale field" when this test was red.
    #[test]
    fn a_run_backed_bucket_promotes_to_tree_mode() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
        let graph = super::super::test_support::test_graph_with_segment_size(
            16,
            BucketLabelKey::directed_from_index(1),
        );
        let vid = VertexId::from(0);
        let first = BucketLabelKey::directed_from_index(2);
        for target in 0..10_000u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    first,
                    crate::labeled::graph::test_support::TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .expect("noise insert");
        }
        let second = BucketLabelKey::directed_from_index(3);
        for target in 0..T_PROMOTE + 1 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    second,
                    crate::labeled::graph::test_support::TestEdge {
                        target: 20_000 + target,
                    },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("second-label edge {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, second)
            .expect("find")
            .expect("the second label has a bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(
            bucket.is_tree_mode(),
            "a run-backed bucket over T_PROMOTE must promote to tree mode"
        );
        // The rows are read before the field is trusted: a reader that sees 1025 rows and a descriptor that says 1026
        // is a descriptor bug, while 1026 rows is a duplicated row.
        // The label reads back whole, which is the point: the rows moved from a run into LTB blocks.
        let mut rows = 0u32;
        let _ = graph
            .visit_edges(
                vid,
                second,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |_, _| {
                    rows += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("read the promoted label");
        assert_eq!(
            rows,
            T_PROMOTE + 1,
            "every promoted row is readable exactly once (descriptor says {})",
            bucket.degree()
        );
    }

    /// No insert writes two rows: the second label's degree and its readable rows both track the inserts made so far,
    /// index by index. This is the assertion that turned "one row is duplicated somewhere" into "the run was
    /// over-allocated by one for the pending edge".
    #[test]
    fn every_insert_on_a_second_label_writes_exactly_one_row() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
        let graph = super::super::test_support::test_graph_with_segment_size(
            16,
            BucketLabelKey::directed_from_index(1),
        );
        let vid = VertexId::from(0);
        let first = BucketLabelKey::directed_from_index(2);
        for target in 0..10_000u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    first,
                    crate::labeled::graph::test_support::TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .expect("noise insert");
        }
        let second = BucketLabelKey::directed_from_index(3);
        for target in 0..T_PROMOTE + 3 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    second,
                    crate::labeled::graph::test_support::TestEdge {
                        target: 20_000 + target,
                    },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("edge {target}: {error}"));
            let vertex = graph.vertices().get(vid);
            let slot = graph
                .find_bucket_slot(&vertex, second)
                .expect("find")
                .expect("bucket");
            let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
            let mut rows = 0u32;
            let _ = graph
                .visit_edges(
                    vid,
                    second,
                    crate::labeled::graph::OutEdgeOrder::Ascending,
                    |_, _| {
                        rows += 1;
                        std::ops::ControlFlow::<()>::Continue(())
                    },
                )
                .expect("read");
            assert_eq!(
                (u64::from(bucket.degree()), rows),
                (u64::from(target) + 1, target + 1),
                "after inserting edge {target}: descriptor={}, readable rows={}, tree={}",
                bucket.degree(),
                rows,
                bucket.is_tree_mode()
            );
        }
    }

    /// Reproduction for the tree-mode inline value read: a valued bucket promoted to tree must read its values back
    /// through the value plane, not through the edge plane's run.
    #[test]
    fn tree_mode_valued_bucket_reads_its_values() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        let width = 8u16;
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, width)
            .expect("declare width");
        let rows = T_PROMOTE + 100;
        for target in 0..rows {
            let value = target.to_le_bytes();
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&value);
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target, &bytes,
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tree_mode(), "the fixture must promote");
        let mut seen = 0u32;
        let _ = graph
            .visit_edges_with_inline_property(
                vid,
                label,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |slot, item| {
                    let expected = slot.raw().to_le_bytes();
                    assert_eq!(
                        &item.inline_property.bytes()[..4],
                        &expected[..],
                        "row {} reads the wrong value",
                        slot.raw()
                    );
                    seen += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("read the tree-mode values");
        assert_eq!(seen, rows);
    }

    /// The bench shape `expand_inline_property_skewed_2k_a_100b` inserts: 2 000 + 100 valued edges on one bucket, so it
    /// promotes to tree mode, and every row's value must still read back - through the point reader the expand
    /// projection uses.
    ///
    /// A promoted valued bucket reads every row back through the point reader.
    ///
    /// The fixture is the bench shape `expand_inline_property_skewed_2k_a_100b`: 2 100 valued edges, so the bucket
    /// promotes to a tree and keeps growing until it spans five property leaves. Three defects had to be fixed for this
    /// to read correctly, and each is now a rule the code states: a leaf payload is written before the pointer that
    /// names it, the descriptor records the value-root region the append just wrote, and the root array is read and
    /// written through one store-typed pair of functions. The row count is asserted before the descriptor is trusted, so
    /// a duplicated row and a stale field cannot be confused again.
    #[test]
    fn a_promoted_valued_bucket_reads_every_row_through_the_point_reader() {
        use crate::labeled::graph::EdgePlacementPolicy;
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        let width = 8u16;
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, width)
            .expect("declare width");
        let rows = 2_100u32;
        for target in 0..rows {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&target.to_le_bytes());
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target, &bytes,
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tree_mode(), "the fixture must promote");
        assert_eq!(
            (bucket.degree(), bucket.stored_slots()),
            (rows, rows),
            "the promoted bucket must be dense: degree and stored must agree for slot addressing"
        );
        // Does the published root name distinct, geometry-sized blocks? Reading it through the shared reader is what a
        // traversal would do, so this is the invariant the read path depends on.
        graph
            .tree_plane(bucket)
            .validate_value_root()
            .unwrap_or_else(|error| {
                panic!("the promoted value root is not a valid root array: {error}")
            });
        for row in 0..bucket.degree() {
            let value = graph
                .read_value_row(&bucket, row)
                .unwrap_or_else(|error| panic!("row {row}: {error}"));
            assert_eq!(value.len(), usize::from(width));
            if row == 1 {
                // Walk the property tree by hand: root entry -> interior payload -> leaf payload, at row 1's offset.
                let depth = bucket.tree_mode_property_depth();
                let root_off = crate::labeled::graph::compact::value_root_address(&bucket);
                let mut root_id = [0u8; 4];
                graph.values().read_bytes(root_off, &mut root_id);
                let root_entry = u32::from_le_bytes(root_id);
                let mut root_bytes = [0u8; 20];
                graph.values().read_bytes(root_off, &mut root_bytes);
                let entries: Vec<u32> = root_bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_le_bytes(*c))
                    .collect();
                let mut id = root_entry;
                if depth >= 2 {
                    let mut child = [0u8; 4];
                    graph
                        .ltb()
                        .read_payload_partial(id, 0, &mut child)
                        .expect("read interior");
                    id = u32::from_le_bytes(child);
                }
                let mut probe = vec![0u8; usize::from(width)];
                graph
                    .ltb()
                    .read_payload_partial(id, 8, &mut probe)
                    .expect("read leaf payload");
                assert_eq!(
                    u32::from_le_bytes(probe[..4].try_into().expect("4 bytes")),
                    row,
                    "depth {depth}, offset {root_off}, entries {entries:?}: root entry {root_entry} -> leaf {id}, offset 8 holds another row"
                );
            }
            assert_eq!(
                u32::from_le_bytes(value[..4].try_into().expect("4 bytes")),
                row,
                "row {row} read a value that is not its own"
            );
        }
    }

    /// Removing an edge from a property-bearing tree bucket reaches its write path.
    ///
    /// The removal carried a debug assertion that tree mode never holds inline properties - true when it was written,
    /// false since property-bearing promotion landed (ADR 0088). A stale debug-only rejection is not a contract, and it
    /// made a real 1024-slot valued tree panic in native debug before reaching any write. This drives the path the
    /// assertion blocked: promote a valued bucket, remove a row, and read the survivors.
    #[test]
    fn a_property_bearing_tree_bucket_removes_a_row() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 8)
            .expect("declare width");
        let rows = T_PROMOTE + 8;
        for target in 0..rows {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&target.to_le_bytes());
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target, &bytes,
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tree_mode(), "the fixture must promote");
        assert!(bucket.inline_property_byte_width() > 0);

        // The removal the assertion used to block.
        graph
            .remove_edge_at_slot(vid, label, 3)
            .expect("remove a row from a property-bearing tree bucket")
            .expect("the row existed");

        let after = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read the descriptor");
        assert_eq!(after.degree, rows - 1, "one row fewer");
        // The removed row is a tombstone at its position, and the tree walk reports it as a hole while a read of the
        // live rows skips it - the visibility split the row walker owns.
        let mut live_rows = 0u32;
        let _ = graph
            .visit_edges(
                vid,
                label,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |_, _| {
                    live_rows += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("read the live rows");
        assert_eq!(
            live_rows,
            rows - 1,
            "a reader sees the survivors, not the tombstone"
        );
        // Every surviving row still reads its own value: a delete writes no value (ADR 0099), so the mapping holds.
        for row in 0..after.degree {
            let value = graph
                .read_value_row(&after, row)
                .unwrap_or_else(|error| panic!("row {row}: {error}"));
            assert_eq!(value.len(), 8);
        }
    }

    /// A property-bearing tree demotes back to a slab when it loses rows, and the survivors keep their values.
    ///
    /// The demotion transcribed its edges and its values with two different selections - the edge visit filtered
    /// tombstones, the value visit kept every slot - so a 1024-position tree with 512 survivors failed
    /// `live property values must cover degree * w bytes`. This drives the threshold: remove until `T_DEMOTE` is reached
    /// and check that the bucket is slab again with exactly the survivors' values.
    #[test]
    fn a_property_bearing_tree_demotes_with_its_values() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_DEMOTE, T_PROMOTE};
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 8)
            .expect("declare width");
        let rows = T_PROMOTE + 2;
        for target in 0..rows {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&target.to_le_bytes());
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target, &bytes,
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tree_mode(), "the fixture must promote");

        // Remove down to the demotion threshold.
        for target in 0..(rows - T_DEMOTE) {
            graph
                .remove_edge_at_slot(vid, label, target)
                .unwrap_or_else(|error| panic!("remove {target}: {error}"))
                .expect("the row existed");
        }
        let after = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read the descriptor");
        assert!(!after.is_tree_mode(), "reaching T_DEMOTE demotes to slab");
        assert_eq!(after.degree, T_DEMOTE);
        // The survivors' values came through the same selection as their edges.
        for row in 0..after.degree {
            let value = graph
                .read_value_row(&after, row)
                .unwrap_or_else(|error| panic!("row {row}: {error}"));
            assert_eq!(value.len(), 8, "row {row} keeps a value");
        }
    }

    /// A demotion that repacks survivors reports every row whose ordinal changed.
    ///
    /// The repack is a physical movement: a tree's sparse ordinals become dense slab positions. A caller that tracks
    /// external identities learns about movement only through `EdgeRemoval::moves`, so a demotion that reported nothing
    /// left an observer pointing at slots that no longer hold those rows.
    #[test]
    fn a_demotion_reports_the_rows_it_moves() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_DEMOTE, T_PROMOTE};
        let graph = crate::labeled::graph::test_support::test_graph_with_segment_size(
            16,
            BucketLabelKey::directed_from_index(1),
        );
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(2);
        let rows = T_PROMOTE + 2;
        for target in 0..rows {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tree_mode(), "the fixture must promote");

        // Remove survivors from the front so the repack has rows to move, and watch the removal that triggers the
        // demotion: it must answer the moves it performed.
        let mut demotion_moves = None;
        for target in 0..(rows - T_DEMOTE) {
            let removal = graph
                .row_plane()
                .remove_row(vid, label, target)
                .unwrap_or_else(|error| panic!("remove {target}: {error}"));
            if let Some(removal) = removal
                && !removal.moves.is_empty()
            {
                demotion_moves = Some(removal.moves);
            }
        }
        let after = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read the descriptor");
        assert!(!after.is_tree_mode(), "reaching T_DEMOTE demotes to slab");
        let moves = demotion_moves.expect("the demotion must report the rows it repacked");
        assert!(
            moves
                .iter()
                .all(|moved| moved.old_slot_index != moved.new_slot_index),
            "a reported move must move something"
        );
        assert!(
            moves.iter().all(|moved| moved.label_id == label),
            "every move belongs to this label row"
        );
    }

    /// Matching removal selects through the tree's own addressing, not the slab's.
    ///
    /// The `Tree` arm of `remove_edge_matching_skip_leaf_cascade_with_move` grouped tree buckets with slab and read
    /// `edge_start + slot` as if the rows lived in edge slots. For a tree bucket that span holds the **edge root array**
    /// (block ids), so a predicate was matched against root entries and the deletion it triggered named a row that never
    /// matched. This drives the case: a promoted valued bucket, one specific target, and the survivors checked afterwards.
    #[test]
    fn a_tree_bucket_removes_the_row_its_predicate_matches() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 8)
            .expect("declare width");
        let rows = T_PROMOTE + 2;
        for target in 0..rows {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&target.to_le_bytes());
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target, &bytes,
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tree_mode(), "the fixture must promote");

        // The predicate names one target in the middle of the tree.
        let wanted = 700u32;
        let removal = graph
            .remove_edge_matching_with_move(vid, label, |candidate| candidate.target == wanted)
            .expect("matching removal")
            .expect("one row matches");
        assert_eq!(
            removal.removed.target, wanted,
            "the removed row is the one the predicate matched, not a root entry"
        );
        let after = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read the descriptor");
        assert_eq!(after.degree, rows - 1, "one row fewer");
        // Every survivor is still there except the removed target.
        let mut seen = Vec::new();
        let _ = graph
            .visit_edges(
                vid,
                label,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |_position, edge| {
                    seen.push(edge.target);
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("read the live rows");
        assert_eq!(seen.len(), (rows - 1) as usize, "survivors only");
        assert!(!seen.contains(&wanted), "the matched row is gone");
    }

    /// Removing one row from a valued slab bucket leaves every survivor's value where it was.
    ///
    /// The values span used to be a dense sequence: a delete copied the whole trailing suffix one block left, which made
    /// draining a bucket quadratic (GAP-LARA-INLINE-DELETE-COST, measured at 454 K instructions per edge for width 8 at
    /// 128 rows). ADR 0099 made a delete write no value at all - the row is tombstoned where it lives and a reader never
    /// asks a tombstoned slot for its value - so the shift must not happen. This asserts the consequence: after deleting
    /// the first row, every survivor still reads its **own** value, in place.
    #[test]
    fn deleting_a_valued_row_moves_no_other_value() {
        use crate::labeled::graph::EdgePlacementPolicy;
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 8)
            .expect("declare width");
        let rows = 8u32;
        let value_of = |target: u32| {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&target.to_le_bytes());
            bytes[4..].copy_from_slice(&(target + 100).to_le_bytes());
            bytes
        };
        for target in 0..rows {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target,
                        &value_of(target),
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let before = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert_eq!(before.degree, rows);
        let values_before: Vec<Vec<u8>> = (0..before.degree)
            .map(|row| graph.read_value_row(&before, row).expect("value"))
            .collect();

        // Delete the first row: the worst case for a suffix shift.
        graph
            .remove_edge_at_slot(vid, label, 0)
            .expect("delete")
            .expect("the row existed");

        let after = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert_eq!(after.degree, rows - 1);
        // The survivors are slots 1..rows and each keeps its own bytes: nothing shifted into them.
        for row in 1..rows {
            let value = graph.read_value_row(&after, row).expect("survivor value");
            assert_eq!(
                value, values_before[row as usize],
                "row {row} still holds its own value after the delete"
            );
        }
    }

    /// The directedness surface reads every storage class, because it now delegates to the canonical visitor.
    ///
    /// It used to be a second implementation that walked slab windows itself: a tiny bucket (no slab window yet) and a
    /// promoted tree bucket (rows in LTB blocks) were outside what it knew, so a caller filtering by directedness could
    /// silently miss them (GAP-2026-09-22-007). This drives both classes through the one surviving implementation.
    #[test]
    fn the_directedness_surface_reads_tiny_and_tree_buckets() {
        use crate::labeled::bucket_label_key::BucketDirectedness;
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};

        let graph = crate::labeled::graph::test_support::test_graph_with_segment_size(
            16,
            BucketLabelKey::directed_from_index(1),
        );
        let vid = VertexId::from(0);
        // A tiny bucket: two directed edges fit inside the descriptor.
        let tiny_label = BucketLabelKey::directed_from_index(2);
        for target in 0..2u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    tiny_label,
                    crate::labeled::graph::test_support::TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("tiny insert {target}: {error}"));
        }
        // A promoted tree bucket on a second label.
        let tree_label = BucketLabelKey::directed_from_index(3);
        for target in 0..(T_PROMOTE + 2) {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    tree_label,
                    crate::labeled::graph::test_support::TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("tree insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let tiny_slot = graph
            .find_bucket_slot(&vertex, tiny_label)
            .expect("find tiny")
            .expect("tiny bucket");
        let tiny = graph
            .buckets()
            .read_label_bucket_slot(tiny_slot)
            .expect("read tiny");
        assert!(tiny.is_tiny_mode(), "the fixture must keep one bucket tiny");
        let tree_slot = graph
            .find_bucket_slot(&vertex, tree_label)
            .expect("find tree")
            .expect("tree bucket");
        let tree = graph
            .buckets()
            .read_label_bucket_slot(tree_slot)
            .expect("read tree");
        assert!(tree.is_tree_mode(), "the fixture must promote one bucket");

        let mut visited = 0usize;
        let _: std::ops::ControlFlow<()> = graph
            .for_each_out_edges_by_directedness(
                vid,
                BucketDirectedness::Directed,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |_edge| {
                    visited += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("visit by directedness");
        assert_eq!(
            visited as u32,
            2 + T_PROMOTE + 2,
            "both the tiny and the promoted tree bucket's edges are visible through this surface"
        );
    }

    /// A promoted tree's value root span is released, not leaked.
    ///
    /// The other half of `GAP-LARA-TINY-GEOMETRY`'s measurement: a width-8 promotion allocated a value root while the vertex's
    /// `inline_property_bytes_allocated_bytes` and the bucket's slab-slot claim both read 0, so that span was released by
    /// nobody and reusable by nobody - a permanent leak per promotion. The accounting is now derived from the buckets
    /// (`invariants::bucket_resident_inline_property_bytes`, the single owner) and promotion reconciles it from what it
    /// published, so the span has an owner and emptying the bucket gives it back.
    ///
    /// Emptying a promoted tree runs through demotion (`degree <= T_DEMOTE`), and the demotion is what gives the span back:
    /// it releases the value root's value-store span through the same owner that charged it, so the vertex ends at zero
    /// rather than holding a span nothing names.
    #[test]
    fn a_promoted_tree_releases_its_value_root_span() {
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
        let graph = crate::labeled::graph::test_support::inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 8)
            .expect("declare width");
        for target in 0..(T_PROMOTE + 2) {
            let mut bytes = vec![0u8; 8];
            bytes[..4].copy_from_slice(&target.to_le_bytes());
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target, &bytes,
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|error| panic!("insert {target}: {error}"));
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tree_mode(), "the fixture must promote");
        let charged = vertex.inline_property_bytes_allocated_bytes();
        assert!(
            charged > 0,
            "a promoted valued tree must charge its vertex for the value root span"
        );
        // Empty the bucket: every row goes, so the root span must go with them.
        let degree = bucket.degree();
        for ordinal in 0..degree {
            let _ = graph.remove_edge_at_slot(vid, label, ordinal);
        }
        let emptied = graph.vertices().get(vid);
        assert_eq!(
            emptied.inline_property_bytes_allocated_bytes(),
            0,
            "the promoted tree's value root span must be released with its rows, not left charged"
        );
    }

    /// A promoted tree's edge root is inside the vertex's own cover, or it lives in a run.
    ///
    /// Two measurement traps, both of which the first version of this test fell into:
    ///
    /// - The cover's base is the **edge-store** address of the vertex's first occupied bucket. `LabeledVertex::base_slot_start`
    ///   is an address in the **bucket** store, a different space; subtracting one from the other invents a violation.
    /// - The published row stores only a *width*, so the claim it makes is `[edge_base, edge_base + stored_slots)`.
    #[test]
    fn a_promoted_tree_keeps_its_edge_root_inside_the_vertex_cover() {
        use crate::labeled::graph::leaf_cover::{SlackRule, VertexLeafCover};
        use crate::labeled::graph::leaf_geometry::LeafGeometry;
        use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
        for width in [0u16, 8] {
            let graph = crate::labeled::graph::test_support::inline_property_test_graph();
            let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
            let label = BucketLabelKey::directed_from_index(2);
            if width > 0 {
                graph
                    .ensure_label_bucket_inline_property_byte_width(vid, label, width)
                    .expect("declare width");
            }
            for target in 0..(T_PROMOTE + 2) {
                let mut bytes = vec![0u8; usize::from(width)];
                if width >= 4 {
                    bytes[..4].copy_from_slice(&target.to_le_bytes());
                }
                graph
                    .insert_edge_skip_leaf_cascade(
                        vid,
                        label,
                        crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                            target, &bytes,
                        ),
                        EdgePlacementPolicy::Insertion,
                    )
                    .unwrap_or_else(|error| panic!("width {width} insert {target}: {error}"));
            }
            let vertex = graph.vertices().get(vid);
            let slot = graph
                .find_bucket_slot(&vertex, label)
                .expect("find")
                .expect("bucket");
            let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
            assert!(
                bucket.is_tree_mode(),
                "width {width}: the fixture must promote"
            );
            let geometry = LeafGeometry::of(&graph);
            let span = geometry.bucket_span(&bucket);
            let published = VertexLeafCover::derive(
                [span],
                SlackRule::NeverShrink {
                    current: vertex.stored_slots,
                },
            );
            assert_eq!(
                published.len(),
                vertex.stored_slots.max(span.len()),
                "width {width}: the published width must be the monotone rule's answer"
            );
            match graph.tree_plane(bucket).edge_root_loc() {
                crate::labeled::graph::tree_plane::EdgeRootLoc::Run { .. } => {}
                crate::labeled::graph::tree_plane::EdgeRootLoc::Slots { start, slots } => {
                    let cover_start = span.start();
                    let cover_end = cover_start.end_of(vertex.stored_slots);
                    assert!(
                        start >= cover_start.get() && start + u64::from(slots) <= cover_end.get(),
                        "width {width}: the edge root [{start}, {}) must lie inside the vertex cover \
                         [{cover_start}, {cover_end})",
                        start + u64::from(slots)
                    );
                }
            }
        }
    }

    #[test]
    fn visit_out_edges_with_raw_still_applies_matches_on_spill_backed_bucket() {
        let graph = inline_property_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            let weight = u16::try_from(target).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &weight.to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.edge_run().is_some());

        let mut visited = Vec::new();
        let mut raw_all = |_bytes: &[u8]| true;
        graph
            .visit_out_edges(
                VertexId::from(0),
                None,
                Some(2),
                Some(&mut raw_all),
                |edge| edge.target < 100 && edge.target % 2 == 0,
                |edge| visited.push(edge),
            )
            .unwrap();

        assert_eq!(
            visited.iter().map(|e| e.target).collect::<Vec<_>>(),
            vec![2, 4]
        );
        for edge in &visited {
            assert_eq!(edge.inline_property_len, 2);
            let b = edge.edge_inline_property_bytes();
            assert_eq!(
                u16::from_le_bytes([b[0], b[1]]),
                u16::try_from(edge.target).unwrap()
            );
        }
    }

    #[test]
    fn out_edge_bucket_index_range_agrees_with_slice_partition() {
        use crate::labeled::BucketDirectedness;

        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::undirected_from_index(3),
                TestEdge { target: 30 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(2),
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(4),
                TestEdge { target: 40 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let v = graph.vertices().get(VertexId::from(0));
        let base = v.base_slot_start();
        let deg = v.degree();
        let full = graph
            .buckets()
            .read_label_bucket_slots_contiguous(base, deg)
            .unwrap();
        let p = full.partition_point(|b| b.bucket_label_key().is_undirected());

        assert_eq!(
            graph
                .out_edge_bucket_index_range_for_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Undirected,
                    OutEdgeOrder::Ascending,
                )
                .unwrap(),
            (0, p as u32)
        );
        assert_eq!(
            graph
                .out_edge_bucket_index_range_for_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Descending,
                )
                .unwrap(),
            (p as u32, deg)
        );

        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_linear_from_start(base, deg)
                .unwrap(),
            p as u32
        );
        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_linear_from_end(base, deg)
                .unwrap(),
            p as u32
        );
        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_hybrid(base, deg)
                .unwrap(),
            p as u32
        );
    }

    #[test]
    fn label_buckets_and_edges_follow_label_order() {
        let graph = test_graph();
        for (label, target) in [(10, 100), (2, 20), (7, 70), (2, 21)] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let labels: Vec<_> = (0..vertex.degree())
            .map(|offset| {
                graph
                    .buckets()
                    .read_label_bucket_slot(vertex.base_slot_start() + u64::from(offset))
                    .unwrap()
                    .bucket_label_key()
                    .raw()
            })
            .collect();
        assert_eq!(labels, vec![2, 7, 10]);
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 20 },
                TestEdge { target: 21 },
                TestEdge { target: 70 },
                TestEdge { target: 100 },
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn bucket_tail_missing_cache_revalidates_cached_slot_label() {
        let graph = test_graph();
        let low = BucketLabelKey::from_raw(10);
        let old_tail = BucketLabelKey::from_raw(20);
        let inserted = BucketLabelKey::from_raw(30);
        let new_tail = BucketLabelKey::from_raw(40);

        graph
            .insert_edge(
                VertexId::from(0),
                low,
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                old_tail,
                TestEdge { target: 20 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        // Populate `last_bucket_lookup` with the old tail label.
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), old_tail)
                .unwrap(),
            vec![TestEdge { target: 20 }]
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let tail_slot = graph.find_bucket_slot(&vertex, old_tail).unwrap().unwrap();
        let tail_bucket = graph.buckets().read_label_bucket_slot(tail_slot).unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(tail_slot, tail_bucket.with_bucket_label_key(new_tail))
            .unwrap();

        graph
            .insert_edge(
                VertexId::from(0),
                inserted,
                TestEdge { target: 30 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
        let labels: Vec<_> = buckets
            .iter()
            .map(|bucket| bucket.bucket_label_key())
            .collect();
        assert_eq!(labels, vec![low, inserted, new_tail]);
    }

    #[test]
    fn empty_middle_label_bucket_does_not_expose_neighbor_edges() {
        let graph = test_graph();
        let low = BucketLabelKey::from_raw(2);
        let middle = BucketLabelKey::from_raw(3);
        let high = BucketLabelKey::from_raw(4);

        graph
            .insert_edge(
                VertexId::from(0),
                low,
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                middle,
                TestEdge { target: 20 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                high,
                TestEdge { target: 30 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), middle, |edge| edge.target == 20)
                .unwrap()
                .is_some()
        );

        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), middle)
                .unwrap(),
            Vec::<TestEdge>::new()
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 10 }, TestEdge { target: 30 }]
        );

        let mut raw_scanned = Vec::new();
        graph
            .visit_out_edges(
                VertexId::from(0),
                None,
                None,
                None,
                |_| true,
                |edge| raw_scanned.push(edge),
            )
            .unwrap();
        assert_eq!(
            raw_scanned,
            vec![TestEdge { target: 10 }, TestEdge { target: 30 }]
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let middle_slot = graph.find_bucket_slot(&vertex, middle).unwrap().unwrap();
        let middle_bucket = graph.buckets().read_label_bucket_slot(middle_slot).unwrap();
        let middle_index = middle_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let successor = graph.bucket_successor_start(&vertex, middle_index).unwrap();
        assert_eq!(middle_bucket.stored_slots(), 0);
        assert!(successor >= middle_bucket.edge_start());

        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn insert_beyond_initial_label_bucket_store_vertex_segment_relocates_buckets() {
        let graph = test_graph();
        for label in 1..=33u16 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge {
                        target: label as u32,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.degree(), 33);
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap().len(), 33);
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn labeled_leaf_segment_is_dense_uses_pma_not_geometry_when_pinned() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        // ADR 0096 §4: tiny birth holds edges inline (no span, no log, no pin).
        // Seed past promotion into log spill (5th edge) so the leaf pins with
        // slab geometry plus log-only PMA contributions.
        for target in [999u32, 1000, 1001, 1002, 1003] {
            graph
                .insert_edge(
                    vid,
                    BucketLabelKey::from_raw(99),
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert!(graph.labeled_leaf_physical_range(vid).unwrap().is_some());
        let pma = graph.labeled_leaf_pma_density(vid);
        let geometry = graph.labeled_leaf_geometry_density(vid);
        let counts = graph.leaf_segment_counts_for_vid(vid);
        assert_eq!(
            counts.actual, 5,
            "PMA actual counts the log-spilled edge too"
        );
        assert!(geometry > 0.0, "promoted bucket owns slab geometry");
        // ADR 0096 §4: the pin floor (whole block) dilutes PMA below geometry
        // (inverted from quota birth, where geometry was zero). The dense
        // verdict still follows PMA, not geometry: spans are full
        // (geometry > threshold) yet the leaf is not dense.
        assert!(pma < geometry, "pin floor dilutes PMA below geometry");
        assert_eq!(
            graph.labeled_leaf_segment_is_dense(vid),
            pma >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY
        );
        assert!(!graph.labeled_leaf_segment_is_dense(vid));
    }

    fn labeled_leaf_rebalance_preserves_scan() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let anchor = BucketLabelKey::from_raw(99);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                vid,
                anchor,
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 0..64u32 {
            graph
                .insert_edge(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let materialized = |label: BucketLabelKey| {
            graph
                .iter_edges_for_label(vid, label)
                .unwrap()
                .into_iter()
                .map(|e| e.target)
                .collect::<Vec<_>>()
        };
        let before_anchor = materialized(anchor);
        let before_road = materialized(road);
        graph.rebalance_cascade_after_labeled_mutation(vid).unwrap();
        assert_eq!(materialized(anchor), before_anchor);
        assert_eq!(materialized(road), before_road);
    }

    #[test]
    fn labeled_leaf_density_triggers_rebalance_not_vertex_span_alone() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        graph
            .insert_edge(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let road = BucketLabelKey::from_raw(2);
        let pma_before = graph.labeled_leaf_pma_density(vid);
        // ADR 0096 §4/§3b: tiny edges are invisible to PMA (no slots reserved).
        // Six seeds promote at the 5th (exact span 5) and spill one edge to the
        // log: PMA actual must rise (log edges count!) while reservations
        // (geometry denominator) stay flat — the log/span firewall the dense
        // decision relies on.
        for target in [1u32, 2, 3, 4, 5, 6] {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let pma_after = graph.labeled_leaf_pma_density(vid);
        assert!(
            pma_after > pma_before,
            "PMA actual/total should rise with live edges"
        );
        let counts = graph.leaf_segment_counts_for_vid(vid);
        assert_eq!(
            counts.actual, 6,
            "PMA actual counts slab and log-spilled edges"
        );
        assert_eq!(
            graph.labeled_leaf_geometry_stored_slots(crate::labeled::graph::LabeledLaraGraph::<
                crate::labeled::graph::test_support::TestEdge,
                crate::VectorMemory,
            >::leaf_index_for_vid(
                vid, graph.edges().header().segment_size
            )),
            5,
            "log spill reserves no new slab geometry"
        );
    }

    #[test]
    fn last_bucket_successor_respects_descriptor_order_cover() {
        use super::super::promote::compact_run_into_prefix_with_growth_retry;
        use crate::{
            VectorMemory,
            traits::{CsrEdge, NO_SPILL_RUN},
        };
        use std::ops::ControlFlow;

        let memories: [VectorMemory; 16] =
            std::array::from_fn(|_| crate::test_support::vector_memory());
        let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = memories.clone();
        let graph = LabeledLaraGraph::new_with_segment_size(
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            i,
            j,
            k,
            l,
            m,
            n,
            o,
            p,
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::from_raw(1),
            32,
        )
        .unwrap();
        let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
        let peer = graph.push_vertex(LabeledVertex::default()).unwrap();
        let tiny = BucketLabelKey::from_raw(2);
        let middle = BucketLabelKey::from_raw(3);
        let last = BucketLabelKey::from_raw(4);
        let assert_rows = |graph: &LabeledLaraGraph<TestEdge, VectorMemory>,
                           owner,
                           label,
                           expected: &[(u32, u32)]| {
            for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                let mut actual = Vec::new();
                assert_eq!(
                    graph
                        .visit_edges_with_inline_property(owner, label, order, |slot, item| {
                            assert!(item.inline_property.bytes().is_empty());
                            actual.push((slot.raw(), item.edge.target));
                            ControlFlow::<()>::Continue(())
                        })
                        .unwrap(),
                    ControlFlow::Continue(())
                );
                let expected: Vec<_> = if order == OutEdgeOrder::Ascending {
                    expected.to_vec()
                } else {
                    expected.iter().copied().rev().collect()
                };
                assert_eq!(actual, expected);
            }
        };
        let assert_covers = |graph: &LabeledLaraGraph<TestEdge, VectorMemory>| {
            let (pin_start, pin_len) = graph.labeled_leaf_physical_range(owner).unwrap().unwrap();
            let mut covers = Vec::new();
            for ordinal in 0..graph.vertices().len() {
                let vertex = graph.vertices().get(VertexId::from(ordinal));
                let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
                let Some(first) = buckets.iter().find(|bucket| !bucket.is_tiny_mode()) else {
                    continue;
                };
                let start = first.edge_start();
                let end = start + u64::from(vertex.stored_slots);
                assert!(pin_start <= start && end <= pin_start + pin_len);
                for bucket in buckets.iter().filter(|bucket| !bucket.is_tiny_mode()) {
                    assert!(!bucket.is_tree_mode());
                    let initialized = graph.edges().initialized_slab_slots(bucket).unwrap();
                    assert!(start <= bucket.edge_start());
                    assert!(bucket.edge_start() + u64::from(initialized) <= end);
                }
                covers.push((start, end));
            }
            for (index, &(start, end)) in covers.iter().enumerate() {
                for &(other_start, other_end) in &covers[index + 1..] {
                    assert!(end <= other_start || other_end <= start);
                }
            }
            for free in graph.edges().free_byte_spans() {
                assert!(
                    free.start_slot + free.len <= pin_start
                        || pin_start + pin_len <= free.start_slot
                );
            }
            crate::labeled::invariants::assert_labeled_layout_invariants(
                graph.vertices(),
                graph.buckets(),
                graph.edges(),
            );
        };
        let fold = |label| {
            let vertex = graph.vertices().get(owner);
            let BucketSearch::Found { slot, bucket } =
                graph.find_bucket(owner, &vertex, label).unwrap()
            else {
                panic!("fold source missing");
            };
            let index =
                LabeledLaraGraph::<TestEdge, VectorMemory>::labeled_bucket_descriptor_index(
                    &vertex, slot,
                )
                .unwrap();
            compact_run_into_prefix_with_growth_retry(&graph, owner, index, bucket).unwrap();
        };
        let tiny_rows: Vec<_> = (0..2).map(|slot| (slot, 2000 + slot)).collect();
        let middle_rows: Vec<_> = (0..32).map(|slot| (slot, 1000 + slot)).collect();
        let last_rows: Vec<_> = (0..8).map(|slot| (slot, 4000 + slot)).collect();
        let peer_rows: Vec<_> = (0..8).map(|slot| (slot, 5000 + slot)).collect();
        for &(_, target) in &tiny_rows {
            graph
                .insert_edge(
                    owner,
                    tiny,
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for &(slot, target) in &middle_rows {
            graph
                .insert_edge(
                    owner,
                    middle,
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            if slot == 7 {
                let vertex = graph.vertices().get(owner);
                let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
                assert_eq!(buckets.len(), 2);
                assert!(buckets[0].is_tiny_mode());
                assert_eq!(buckets[1].edge_start(), 256);
                assert_eq!(buckets[1].stored_slots(), 4);
                assert_eq!(vertex.stored_slots, 5);
                assert_eq!(
                    LabeledLaraGraph::<TestEdge, VectorMemory>::calculate_label_edge_span_positions_by_resident_slots(
                        256, 37, &buckets, Some(1), 4,
                    )
                    .unwrap(),
                    vec![256, 256],
                    "leading tiny anchors must preserve the requested first resident origin"
                );
                fold(middle);
            }
        }
        // Multi-label insertion retains later rows in the run; finish the middle prefix
        // through the same owning compactor, so the last bucket has only true tail slack.
        fold(middle);
        for &(_, target) in &last_rows {
            graph
                .insert_edge(
                    owner,
                    last,
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_rows(&graph, owner, tiny, &tiny_rows);
        assert_rows(&graph, owner, middle, &middle_rows);
        assert_rows(&graph, owner, last, &last_rows);
        assert_covers(&graph);
        let vertex = graph.vertices().get(owner);
        let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
        assert_eq!(buckets.len(), 3);
        assert!(buckets[0].is_tiny_mode());
        assert!(!buckets[1].is_tiny_mode());
        assert_eq!(buckets[1].stored_slots(), 32);
        let source = buckets[2];
        assert!(!source.is_tiny_mode() && !source.is_tree_mode());
        assert_eq!(source.bucket_label_key(), last);
        assert!(buckets[1].edge_start() < source.edge_start());
        let run = graph.edges().spill_mint(source.edge_spill_run()).unwrap();
        let used = graph.edges().spill_used_len(run);
        let initialized = graph.edges().initialized_slab_slots(&source).unwrap();
        assert!(initialized > 0 && used > 0);
        assert_eq!(initialized + used, 8);
        let end = buckets[1].edge_start() + u64::from(vertex.stored_slots);
        assert!(source.edge_start() + u64::from(initialized + used) > end);
        println!(
            "TAIL0404 SOURCE first={} last={} reservation={} prefix={initialized} run={used} end={end}",
            buckets[1].edge_start(),
            source.edge_start(),
            vertex.stored_slots
        );
        assert_eq!(
            graph.bucket_successor_start(&vertex, 2).unwrap(),
            end,
            "last successor must end at descriptor-order cover"
        );
        let counts = graph.leaf_segment_counts_for_vid(owner);
        assert_eq!(graph.edges().header().num_edges, 42);
        assert_eq!(counts.actual, 40);
        let pin = graph.labeled_leaf_physical_range(owner).unwrap().unwrap();
        assert_eq!(counts.total, pin.1 as i64);
        let free = graph.edges().free_byte_spans();
        let run_rows: Vec<_> = graph
            .edges()
            .spill_asc_indices(run)
            .into_iter()
            .map(|index| graph.edges().spill_edge_at(run, index).unwrap())
            .collect();
        let mut pin_bytes = vec![0; pin.1 as usize * TestEdge::BYTES];
        graph
            .edges()
            .read_slots_contiguous_bytes(pin.0, &mut pin_bytes);
        let error = graph
            .compact_label_bucket_overflow_to_slab(owner, 2, source)
            .unwrap_err();
        assert!(matches!(
            error,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
        assert_eq!(graph.vertices().get(owner), vertex);
        assert_eq!(graph.read_vertex_label_buckets(&vertex).unwrap(), buckets);
        assert_eq!(graph.edges().spill_used_len(run), used);
        assert_eq!(
            graph
                .edges()
                .spill_asc_indices(run)
                .into_iter()
                .map(|index| graph.edges().spill_edge_at(run, index).unwrap())
                .collect::<Vec<_>>(),
            run_rows
        );
        assert_eq!(graph.edges().header().num_edges, 42);
        assert_eq!(graph.leaf_segment_counts_for_vid(owner), counts);
        assert_eq!(graph.labeled_leaf_physical_range(owner).unwrap(), Some(pin));
        assert_eq!(graph.edges().free_byte_spans(), free);
        let mut after = vec![0; pin_bytes.len()];
        graph.edges().read_slots_contiguous_bytes(pin.0, &mut after);
        assert_eq!(
            after, pin_bytes,
            "rejected fold changed assigned payload bytes"
        );
        assert_rows(&graph, owner, last, &last_rows);
        fold(last);
        let grown = graph.vertices().get(owner);
        let last_bucket = graph.read_vertex_label_buckets(&grown).unwrap()[2];
        assert_eq!(last_bucket.stored_slots(), 8);
        assert_eq!(last_bucket.edge_spill_run(), NO_SPILL_RUN);
        assert_eq!(graph.edges().header().num_edges, 42);
        assert_eq!(graph.leaf_segment_counts_for_vid(owner), counts);
        assert_covers(&graph);
        for &(_, target) in &peer_rows {
            graph
                .insert_edge(
                    peer,
                    last,
                    TestEdge { target },
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_rows(&graph, owner, tiny, &tiny_rows);
        assert_rows(&graph, owner, middle, &middle_rows);
        assert_rows(&graph, owner, last, &last_rows);
        assert_rows(&graph, peer, last, &peer_rows);
        assert_eq!(graph.edges().header().num_edges, 50);
        assert_eq!(graph.leaf_segment_counts_for_vid(owner).actual, 48);
        assert_eq!(graph.leaf_segment_counts_for_vid(owner).total, counts.total);
        assert_covers(&graph);
        let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = memories.clone();
        drop(graph);
        let reopened = LabeledLaraGraph::<TestEdge, _>::init(
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            i,
            j,
            k,
            l,
            m,
            n,
            o,
            p,
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        assert_rows(&reopened, owner, tiny, &tiny_rows);
        assert_rows(&reopened, owner, middle, &middle_rows);
        assert_rows(&reopened, owner, last, &last_rows);
        assert_rows(&reopened, peer, last, &peer_rows);
        assert_covers(&reopened);
        assert_eq!(reopened.edges().header().num_edges, 50);
        assert_eq!(reopened.leaf_segment_counts_for_vid(owner).actual, 48);
        reopened
            .insert_edge(
                peer,
                last,
                TestEdge { target: 9999 },
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let mut peer_after = peer_rows;
        peer_after.push((8, 9999));
        assert_rows(&reopened, peer, last, &peer_after);
        assert_rows(&reopened, owner, tiny, &tiny_rows);
        assert_rows(&reopened, owner, middle, &middle_rows);
        assert_rows(&reopened, owner, last, &last_rows);
        assert_eq!(reopened.edges().header().num_edges, 51);
        assert_eq!(reopened.leaf_segment_counts_for_vid(owner).actual, 49);
        assert_eq!(
            reopened.leaf_segment_counts_for_vid(owner).total,
            counts.total
        );
        assert_covers(&reopened);
        println!("TAIL0404 REOPEN writable=true rows=51 actual=49");
    }
}
