//! Labeled graph `values` implementation.

use crate::{
    VertexId,
    labeled::slot_index::checked_add_slot_index,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
    },
    lara::{edge_payload::PayloadLogWriteError, operation_error::LaraOperationError},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{BucketSearch, LabeledLaraGraph};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn bucket_resident_payload_bytes(&self, bucket: &LabelBucket) -> u64 {
        crate::labeled::invariants::bucket_resident_payload_bytes(bucket)
    }

    pub(super) fn bucket_resident_payload_slots(&self, bucket: &LabelBucket) -> u32 {
        crate::labeled::invariants::bucket_resident_payload_slots(bucket)
    }

    pub(super) fn bucket_reserved_edge_slots(&self, src: VertexId, bucket: &LabelBucket) -> u32 {
        let _ = src;
        bucket.stored_slots
    }

    pub(super) fn bucket_edge_log_slots(&self, src: VertexId, bucket: &LabelBucket) -> u32 {
        if bucket.overflow_log_head() >= 0 {
            let leaf = self.payload_log_leaf(src);
            self.edges
                .overflow_log_chain_len(leaf, bucket.overflow_log_head())
        } else {
            0
        }
    }

    pub(super) fn bucket_slab_prefix_slots(&self, src: VertexId, bucket: &LabelBucket) -> u32 {
        bucket
            .stored_slots
            .saturating_sub(self.bucket_edge_log_slots(src, bucket))
    }

    pub(super) fn bucket_resident_payload_slots_for(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> u32 {
        if !bucket.is_payload_allocated() || bucket.payload_byte_width() == 0 {
            return 0;
        }
        let payload_log_len = u32::from(bucket.payload_log_len());
        if payload_log_len > 0 {
            self.bucket_reserved_edge_slots(src, bucket)
                .saturating_sub(payload_log_len)
        } else {
            self.bucket_reserved_edge_slots(src, bucket)
                .max(bucket.degree())
        }
    }

    pub(super) fn bucket_resident_payload_bytes_for(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> u64 {
        u64::from(self.bucket_resident_payload_slots_for(src, bucket))
            .saturating_mul(u64::from(bucket.payload_byte_width()))
    }

    pub(super) fn reconcile_vertex_payload_allocated_bytes(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<(), LabeledOperationError> {
        let total: u64 = buckets
            .iter()
            .map(|b| self.bucket_resident_payload_bytes_for(src, b))
            .try_fold(0u64, |acc, bytes| {
                acc.checked_add(bytes)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)
            })?;
        if vertex.payload_allocated_bytes() == total {
            debug_assert_eq!(
                vertex.payload_allocated_bytes(),
                total,
                "vertex {src:?} payload_allocated_bytes must match bucket resident sum"
            );
            return Ok(());
        }
        let updated = vertex
            .try_with_payload_allocated_bytes(total)
            .map_err(LabeledOperationError::from)?;
        self.vertices.set(src, &updated);
        debug_assert_eq!(
            self.vertices.get(src).payload_allocated_bytes(),
            total,
            "vertex {src:?} payload_allocated_bytes must match bucket resident sum after reconcile"
        );
        Ok(())
    }

    pub(super) fn payload_log_leaf(&self, src: VertexId) -> u32 {
        u32::from(src) / self.edges.header().segment_size.max(1)
    }

    pub(super) fn vertex_payload_spans_need_sync_after_rewrite(
        old_buckets: &[LabelBucket],
        moved: bool,
        old_alloc: u32,
        compact: bool,
    ) -> bool {
        if moved && old_alloc > 0 {
            return true;
        }
        if compact {
            return old_buckets.iter().any(|b| b.is_payload_allocated());
        }
        old_buckets
            .iter()
            .any(|b| b.is_payload_allocated() && b.payload_log_len() > 0)
    }

    pub(super) fn read_bucket_payloads_slab_dense(
        &self,
        bucket: &LabelBucket,
    ) -> Option<Vec<Vec<u8>>> {
        if !super::super::invariants::bucket_dense_slab_payload_readable(bucket) {
            return None;
        }
        let degree = bucket.degree() as usize;
        let width = usize::from(bucket.payload_byte_width());
        let nbytes = degree.checked_mul(width)?;
        let mut raw = vec![0u8; nbytes];
        self.values.read_bytes(bucket.payload_offset(), &mut raw);
        Some(
            raw.chunks(width)
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>(),
        )
    }

    pub(super) fn collect_bucket_payloads_asc_order(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<Vec<Vec<u8>>, LabeledOperationError> {
        Ok(self
            .collect_bucket_payload_slots_asc_order(src, vertex, bucket_index, bucket)?
            .into_iter()
            .map(|(_, payload)| payload)
            .collect())
    }

    pub(super) fn collect_bucket_payload_slots_asc_order(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<Vec<(u32, Vec<u8>)>, LabeledOperationError> {
        if !bucket.is_payload_allocated() || bucket.payload_byte_width() == 0 {
            return Ok(Vec::new());
        }
        if let Some(dense) = self.read_bucket_payloads_slab_dense(bucket) {
            return dense
                .into_iter()
                .enumerate()
                .map(|(slot, payload)| {
                    let slot = u32::try_from(slot)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                    Ok((slot, payload))
                })
                .collect();
        }
        let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let successor = self.bucket_successor_start(vertex, bucket_index)?;
        let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
        let edges = self
            .edges
            .asc_out_edges(&acc, VertexId::from(0))
            .map_err(LabeledOperationError::from)?;
        let log_chains =
            (bucket.overflow_log_head() >= 0).then(|| self.bucket_log_chains(src, bucket));
        let mut out = Vec::with_capacity(edges.len());
        for edge in edges {
            let slot_index = edge.edge_slot_index_raw();
            out.push((
                slot_index,
                self.read_bucket_payload_for_edge(src, bucket, &edge, log_chains.as_ref())?,
            ));
        }
        Ok(out)
    }

    pub(super) fn read_bucket_payload_for_edge(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        edge: &E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> Result<Vec<u8>, LabeledOperationError> {
        let width = bucket.payload_byte_width();
        if width == 0 {
            return Ok(Vec::new());
        }
        let slot_index = edge.edge_slot_index_raw();
        if bucket.payload_log_head() < 0 {
            let mut buf = vec![0u8; usize::from(width)];
            let offset = super::super::invariants::payload_byte_offset_at_slot(bucket, slot_index)?;
            self.values.read_bytes(offset, &mut buf);
            return Ok(buf);
        }

        let log_len = u32::from(bucket.payload_log_len());
        let reserved_slots = self.bucket_reserved_edge_slots(src, bucket);
        let slab_payload_slots = reserved_slots
            .checked_sub(log_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if slot_index < slab_payload_slots {
            let mut buf = vec![0u8; usize::from(width)];
            let offset = super::super::invariants::payload_byte_offset_at_slot(bucket, slot_index)?;
            self.values.read_bytes(offset, &mut buf);
            return Ok(buf);
        }
        let asc_log_index = slot_index
            .checked_sub(slab_payload_slots)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let mut buf = vec![0u8; usize::from(width)];
        if let Some((_, payload_chain)) = log_chains {
            self.values
                .read_payload_log_chain_entry(
                    self.payload_log_leaf(src),
                    payload_chain,
                    asc_log_index,
                    width,
                    &mut buf,
                )
                .map_err(LabeledOperationError::from)?;
        } else {
            self.values.read_payload_log_asc_index(
                self.payload_log_leaf(src),
                bucket.payload_log_head(),
                asc_log_index,
                width,
                &mut buf,
            )?;
        }
        Ok(buf)
    }

    pub(super) fn write_edge_payload_to_log(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        _edge_entry_idx: i32,
        edge: &E,
    ) -> Result<LabelBucket, LabeledOperationError> {
        let width = bucket.payload_byte_width();
        if width == 0 {
            return Ok(*bucket);
        }
        let src_i32 = i32::try_from(u32::from(src))
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let entry_idx = self
            .values
            .append_payload_log_entry(
                self.payload_log_leaf(src),
                bucket.payload_log_head(),
                src_i32,
                width,
                edge.edge_payload_bytes(),
            )
            .map_err(LabeledOperationError::from)?;
        let next_len = bucket
            .payload_log_len()
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        bucket
            .try_with_payload_log(
                i32::try_from(entry_idx)
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                next_len,
            )
            .map_err(LabeledOperationError::from)
    }

    pub(super) fn mark_payload_log_slot_dead(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        slot_index: u32,
    ) -> Result<(), LabeledOperationError> {
        if bucket.payload_log_head() < 0 || bucket.payload_log_len() == 0 {
            return Ok(());
        }
        let reserved_slots = self.bucket_reserved_edge_slots(src, bucket);
        let payload_log_len = u32::from(bucket.payload_log_len());
        let payload_log_base = reserved_slots
            .checked_sub(payload_log_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if slot_index < payload_log_base {
            return Ok(());
        }
        let asc_log_index = slot_index
            .checked_sub(payload_log_base)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let leaf = self.payload_log_leaf(src);
        let chain = self
            .values
            .payload_log_chain_asc_indices(leaf, bucket.payload_log_head());
        let Some(&entry_idx) = chain.get(asc_log_index as usize) else {
            return Ok(());
        };
        self.values
            .mark_payload_log_entry_dead(leaf, entry_idx)
            .map_err(LabeledOperationError::from)
    }

    pub(super) fn write_edge_payload_after_insert(
        &self,
        src: VertexId,
        bucket_slot: u64,
        mut bucket: LabelBucket,
        mut prev_payload_slots: u32,
        mut slot_index: u32,
        edge: &E,
        prefer_log: bool,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.payload_byte_width() == 0 || edge.edge_payload_byte_width() == 0 {
            return Ok(bucket);
        }
        if prefer_log || bucket.payload_log_len() > 0 {
            match self.write_edge_payload_to_log(src, &bucket, -1, edge) {
                Ok(updated) => return Ok(updated),
                Err(LabeledOperationError::PayloadLogWrite(
                    PayloadLogWriteError::SegmentLogFull,
                )) => {
                    self.rebalance_payload_log_leaf_for_labeled(src)?;
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    prev_payload_slots = self.bucket_resident_payload_slots_for(src, &bucket);
                    slot_index = bucket.degree().saturating_sub(1);
                    if prefer_log {
                        return self.write_edge_payload_to_log(src, &bucket, -1, edge);
                    }
                }
                Err(err) => return Err(err),
            }
        }
        let bucket =
            self.ensure_bucket_payload_span(src, bucket_slot, bucket, prev_payload_slots)?;
        self.write_edge_payload_at_slot(&bucket, slot_index, edge)?;
        Ok(bucket)
    }

    pub(super) fn ensure_bucket_payload_schema_for_insert(
        &self,
        bucket: LabelBucket,
        edge_payload_width: u16,
    ) -> Result<LabelBucket, LabeledOperationError> {
        let bucket_width = bucket.payload_byte_width();
        if bucket_width == edge_payload_width {
            return Ok(bucket);
        }
        Err(LabeledOperationError::PayloadByteWidthMismatch {
            bucket_width,
            edge_payload_width,
        })
    }

    pub(super) fn release_bucket_payload_span(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Result<(), LabeledOperationError> {
        let len = self.bucket_resident_payload_bytes_for(src, bucket);
        if len == 0 {
            return Ok(());
        }
        self.values
            .retire_byte_span(bucket.payload_offset(), len)
            .map_err(LabeledOperationError::from)?;
        let vertex = self.vertices.get(src);
        let new_alloc = vertex.payload_allocated_bytes().saturating_sub(len);
        let updated = vertex
            .try_with_payload_allocated_bytes(new_alloc)
            .map_err(LabeledOperationError::from)?;
        self.vertices.set(src, &updated);
        Ok(())
    }

    pub(super) fn read_bucket_payloads_in_edge_slot_order(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<Vec<Vec<u8>>, LabeledOperationError> {
        self.collect_bucket_payloads_asc_order(src, vertex, bucket_index, bucket)
    }

    pub(super) fn ensure_bucket_payload_byte_width_on_slot(
        &self,
        _src: VertexId,
        _bucket_slot: u64,
        bucket: LabelBucket,
        payload_byte_width: u16,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.payload_byte_width() == payload_byte_width {
            return Ok(bucket);
        }
        let schema_unset = bucket.payload_byte_width() == 0
            && bucket.degree() == 0
            && bucket.stored_slots == 0
            && bucket.overflow_log_head() < 0
            && bucket.payload_log_head() < 0
            && bucket.payload_log_len() == 0;
        if !schema_unset {
            return Err(LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: bucket.payload_byte_width(),
                edge_payload_width: payload_byte_width,
            });
        }
        Ok(bucket.with_payload_byte_width(payload_byte_width))
    }

    /// Ensures that the bucket for `label_id` can store payload slots of `payload_byte_width`.
    pub fn ensure_label_bucket_payload_byte_width(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        payload_byte_width: u16,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let (bucket_slot, bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let bucket = self.ensure_bucket_payload_byte_width_on_slot(
            src,
            bucket_slot,
            bucket,
            payload_byte_width,
        )?;
        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        Ok(())
    }

    pub(super) fn ensure_bucket_payload_span(
        &self,
        src: VertexId,
        bucket_slot: u64,
        mut bucket: LabelBucket,
        prev_stored_slots: u32,
    ) -> Result<LabelBucket, LabeledOperationError> {
        let width = bucket.payload_byte_width();
        let needed_slots = self.bucket_resident_payload_slots_for(src, &bucket);
        if width == 0 || needed_slots == 0 {
            return Ok(bucket);
        }
        let needed_bytes = u64::from(needed_slots)
            .checked_mul(u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let had_bytes = u64::from(prev_stored_slots)
            .checked_mul(u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let tail = self.values.header().slab_occupied_tail;
        let old_offset = bucket.payload_offset();
        let span_ends_at_tail = old_offset
            .checked_add(had_bytes)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?
            == tail;
        if needed_bytes <= had_bytes && span_ends_at_tail {
            return Ok(bucket);
        }
        let extra = needed_bytes.saturating_sub(had_bytes);
        let alloc_delta;

        if had_bytes == 0 {
            // First span for this bucket: bump the occupied tail when the slab already
            // has bytes so we do not place a second bucket at offset 0.
            let offset = if tail == 0 {
                self.values
                    .allocate_byte_span(needed_bytes)
                    .map_err(LabeledOperationError::from)?
            } else {
                self.values
                    .append_byte_span(needed_bytes)
                    .map_err(LabeledOperationError::from)?
            };
            bucket = bucket
                .with_payload_offset(offset)
                .try_with_payload_log_head(-1)
                .map_err(LabeledOperationError::from)?;
            alloc_delta = needed_bytes;
        } else if span_ends_at_tail
            && self
                .values
                .grow_byte_span_in_place(old_offset, had_bytes, needed_bytes)
                .map_err(LabeledOperationError::from)?
        {
            alloc_delta = extra;
        } else {
            let mut old_buf = vec![
                0u8;
                usize::try_from(had_bytes).map_err(|_| {
                    LaraOperationError::CollectAllocationOverflow
                })?
            ];
            self.values.read_bytes(old_offset, &mut old_buf);
            let new_offset = self
                .values
                .append_byte_span(needed_bytes)
                .map_err(LabeledOperationError::from)?;
            self.values
                .write_bytes(new_offset, &old_buf)
                .map_err(LabeledOperationError::from)?;
            if extra > 0 {
                let pad = vec![
                    0u8;
                    usize::try_from(extra)
                        .map_err(|_| { LaraOperationError::CollectAllocationOverflow })?
                ];
                self.values
                    .write_bytes(
                        new_offset
                            .checked_add(had_bytes)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                        &pad,
                    )
                    .map_err(LabeledOperationError::from)?;
            }
            if new_offset != old_offset {
                self.values
                    .retire_byte_span(old_offset, had_bytes)
                    .map_err(LabeledOperationError::from)?;
            }
            bucket = bucket.with_payload_offset(new_offset);
            alloc_delta = extra;
            debug_assert_eq!(bucket.payload_offset(), new_offset);
        }

        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;

        if alloc_delta > 0 {
            let vertex = self.vertices.get(src);
            let new_alloc = vertex
                .payload_allocated_bytes()
                .checked_add(alloc_delta)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let updated = vertex
                .try_with_payload_allocated_bytes(new_alloc)
                .map_err(LabeledOperationError::from)?;
            self.vertices.set(src, &updated);
        }
        if bucket.is_payload_allocated() {
            let vertex = self.vertices.get(src);
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            self.reconcile_vertex_payload_allocated_bytes(src, &vertex, &buckets)?;
        }
        Ok(bucket)
    }

    /// Updates the edge-payload payload for one live edge at `slot_index` inside `label_id`.
    pub fn update_edge_payload_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
        edge: E,
    ) -> Result<bool, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex)
                || slot_index >= vertex.stored_degree()
            {
                return Ok(false);
            }
            let edge_slot = checked_add_slot_index(vertex.base_slot_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let current = self.edges.read_slot(edge_slot);
            if current.is_tombstone_edge() {
                return Ok(false);
            }
            if edge.edge_payload_byte_width() != 0 {
                return Ok(false);
            }
            return Ok(true);
        }
        let (slot, mut bucket) = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => (slot, bucket),
            BucketSearch::Missing { .. } => return Ok(false),
        };
        if bucket.payload_log_len() > 0 {
            self.rebalance_payload_log_leaf_for_labeled(src)?;
            bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        if !self.labeled_bucket_slot_is_live_edge(
            src,
            &vertex,
            bucket_index,
            slot,
            &bucket,
            slot_index,
        )? {
            return Ok(false);
        }
        let edge_payload_width = edge.edge_payload_byte_width();
        if edge_payload_width != bucket.payload_byte_width() {
            return Err(LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: bucket.payload_byte_width(),
                edge_payload_width,
            });
        }
        if edge_payload_width != 0 {
            let prev_payload_slots = self.bucket_resident_payload_slots_for(src, &bucket);
            bucket = self.ensure_bucket_payload_span(src, slot, bucket, prev_payload_slots)?;
            self.write_edge_payload_at_slot(&bucket, slot_index, &edge)?;
        }
        self.buckets.write_label_bucket_slot(slot, bucket)?;
        self.invalidate_bucket_lookup_for_label(src, label_id);
        Ok(true)
    }

    fn labeled_bucket_slot_is_live_edge(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: &LabelBucket,
        slot_index: u32,
    ) -> Result<bool, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.overflow_log_head() < 0 {
            if slot_index >= bucket.stored_slots {
                return Ok(false);
            }
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let current = self.edges.read_slot(edge_slot);
            return Ok(!current.is_deleted_slot() && !current.is_tombstone_edge());
        }

        let successor = self.bucket_successor_start_after_bucket(vertex, bucket_index, bucket)?;
        let acc = LabelEdgeSpanAccess::new(&self.buckets, bucket_slot, successor, src);
        for edge in self.edges.asc_out_edges(&acc, VertexId::from(0))? {
            if edge.edge_slot_index_raw() == slot_index {
                return Ok(!edge.is_tombstone_edge());
            }
        }
        Ok(false)
    }

    pub(super) fn write_edge_payload_at_slot(
        &self,
        bucket: &LabelBucket,
        slot_index: u32,
        edge: &E,
    ) -> Result<(), LabeledOperationError> {
        let width = bucket.payload_byte_width();
        if width == 0 {
            return Ok(());
        }
        let edge_payload_width = edge.edge_payload_byte_width();
        if edge_payload_width == 0 {
            return Ok(());
        }
        if edge_payload_width != width {
            return Err(LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: width,
                edge_payload_width,
            });
        }
        let offset = super::super::invariants::payload_byte_offset_at_slot(bucket, slot_index)?;
        self.values
            .write_payload_slot(offset, width, edge.edge_payload_bytes())
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }

    pub(super) fn attach_edge_payload(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        bucket: LabelBucket,
        slot_index: u32,
        edge: E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> Result<E, LabeledOperationError> {
        if !bucket.is_payload_allocated() {
            return Ok(edge);
        }
        let width = bucket.payload_byte_width();
        let edge = edge.with_slot_index(slot_index);
        let buf = self.read_bucket_payload_for_edge(src, &bucket, &edge, log_chains)?;
        Ok(edge.with_stored_payload_bytes(width, &buf))
    }

    pub(super) fn bucket_payload_log_chains_opt(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Option<(Vec<u32>, Vec<u32>)> {
        if bucket.is_payload_allocated()
            && (bucket.overflow_log_head() >= 0 || bucket.payload_log_head() >= 0)
        {
            Some(self.bucket_log_chains(src, bucket))
        } else {
            None
        }
    }

    pub(super) fn collect_bucket_payloads_before_edge_rewrite(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<Vec<Vec<Vec<u8>>>, LabeledOperationError> {
        let mut saved = Vec::with_capacity(buckets.len());
        for (index, bucket) in buckets.iter().enumerate() {
            let values = match self.read_bucket_payloads_slab_dense(bucket) {
                Some(v) => v,
                None => {
                    self.read_bucket_payloads_in_edge_slot_order(src, vertex, index as u32, bucket)?
                }
            };
            if bucket.payload_byte_width() > 0 {
                let live = usize::try_from(bucket.degree()).unwrap_or(usize::MAX);
                debug_assert_eq!(
                    values.len(),
                    live,
                    "collect before rewrite: one value per live edge (bucket {index})"
                );
            }
            saved.push(values);
        }
        Ok(saved)
    }

    pub(super) fn sync_vertex_payload_spans_after_edge_rewrite(
        &self,
        src: VertexId,
        old_buckets: &[LabelBucket],
        new_buckets: &[LabelBucket],
        saved: &[Vec<Vec<u8>>],
    ) -> Result<(), LabeledOperationError> {
        if old_buckets.len() != new_buckets.len() || saved.len() != old_buckets.len() {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let vertex = self.vertices.get(src);
        for old in old_buckets {
            self.release_bucket_payload_span(src, old)?;
        }
        for (index, new_bucket) in new_buckets.iter().enumerate() {
            if new_bucket.payload_byte_width() == 0 {
                continue;
            }
            let live = usize::try_from(new_bucket.degree()).unwrap_or(usize::MAX);
            debug_assert_eq!(
                saved[index].len(),
                live,
                "sync after rewrite: saved value count must match new live degree (bucket {index})"
            );
            let slot = Self::labeled_vertex_bucket_slot(&vertex, index as u32)?;
            let mut bucket = new_bucket.with_payload_offset(0);
            bucket = self.ensure_bucket_payload_span(src, slot, bucket, 0)?;
            let width = bucket.payload_byte_width();
            let flat_len = saved[index]
                .len()
                .checked_mul(usize::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if flat_len > 0 {
                let mut flat = Vec::with_capacity(flat_len);
                for bytes in &saved[index] {
                    flat.extend_from_slice(bytes);
                }
                self.values
                    .write_bytes(bucket.payload_offset(), &flat)
                    .map_err(LabeledOperationError::from)?;
            }
            self.buckets.write_label_bucket_slot(slot, bucket)?;
        }
        Ok(())
    }

    pub(super) fn ensure_bucket_slack_insert_when_peers_have_values(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(src);
        if vertex.degree() == 0 || vertex.payload_allocated_bytes() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let has_live_value_span = buckets
            .iter()
            .any(|b| b.is_payload_allocated() && self.bucket_resident_payload_bytes(b) > 0);
        if has_live_value_span {
            return self.reconcile_vertex_payload_allocated_bytes(src, &vertex, &buckets);
        }
        if vertex.payload_allocated_bytes() > 0 {
            return Ok(());
        }
        if buckets.iter().any(|b| b.is_payload_allocated()) {
            self.rebalance_vertex_edge_span(src, None, 1, true)?;
            let vertex = self.vertices.get(src);
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            let total_live = buckets.iter().try_fold(0u32, |acc, bucket| {
                acc.checked_add(bucket.degree())
                    .ok_or(LaraOperationError::RowDegreeOverflow)
            })?;
            if vertex.stored_slots.saturating_sub(total_live) < 2 {
                self.rebalance_vertex_edge_span(src, None, 1, true)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_assert_bucket_payloads_follow_edge_slab_order(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        use crate::labeled::access::LabelEdgeSpanAccess;
        use crate::labeled::invariants::payload_byte_offset_at_slot;

        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        for (bucket_index, bucket) in buckets.iter().enumerate() {
            if !bucket.is_payload_allocated() || bucket.payload_byte_width() == 0 {
                continue;
            }
            let bucket_index = u32::try_from(bucket_index)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let slot_payloads =
                self.collect_bucket_payload_slots_asc_order(src, &vertex, bucket_index, bucket)?;

            let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            let successor = self.bucket_successor_start(&vertex, bucket_index)?;
            let acc = LabelEdgeSpanAccess::new(&self.buckets, bucket_slot, successor, src);
            let mut edge_slots = Vec::new();
            for edge in self
                .edges
                .asc_out_edges(&acc, VertexId::from(0))
                .map_err(LabeledOperationError::from)?
            {
                if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                    continue;
                }
                edge_slots.push(edge.edge_slot_index_raw());
            }

            let payload_slots: Vec<u32> = slot_payloads.iter().map(|(slot, _)| *slot).collect();
            assert_eq!(
                payload_slots,
                edge_slots,
                "label {:?}: payload slots must follow asc edge slab order",
                bucket.bucket_label_key()
            );

            let width = usize::from(bucket.payload_byte_width());
            for (slot, expected) in slot_payloads {
                let offset = payload_byte_offset_at_slot(bucket, slot)?;
                let mut at_offset = vec![0u8; width];
                self.values.read_bytes(offset, &mut at_offset);
                assert_eq!(
                    at_offset,
                    expected,
                    "label {:?} slot {slot}: payload bytes must live at dense offset",
                    bucket.bucket_label_key()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{BucketSearch, *};
    use crate::VertexId;

    #[test]
    fn edge_payloads_round_trip_via_unchecked_label_iteration() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(1), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &1u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(2, &100u16.to_le_bytes()),
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        if let BucketSearch::Found { bucket, .. } =
            graph.find_bucket(VertexId::from(0), &vertex, road).unwrap()
        {
            let mut raw = vec![0u8; 4];
            graph.values().read_bytes(bucket.payload_offset(), &mut raw);
            assert_eq!(u16::from_le_bytes([raw[0], raw[1]]), 1);
            assert_eq!(u16::from_le_bytes([raw[2], raw[3]]), 100);
        }
        let mut edges = Vec::new();
        graph
            .for_each_edges_for_label_unchecked(VertexId::from(0), road, |edge| {
                edges.push(edge);
            })
            .unwrap();
        assert_eq!(edges.len(), 2);
        let mut weights: Vec<u16> = edges
            .iter()
            .filter(|e| e.payload_len == 2)
            .map(|e| {
                let b = e.edge_payload_bytes();
                u16::from_le_bytes([b[0], b[1]])
            })
            .collect();
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 100]);
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn edge_payloads_survive_middle_vertex_insert() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(1), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &1u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(1),
                road,
                PayloadTestEdge::with_bytes(2, &1u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(2, &100u16.to_le_bytes()),
            )
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    let b = edge.edge_payload_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 100]);
    }

    #[test]
    fn edge_payloads_preserved() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1u32, 3u16), (2, 7u16), (3, 11)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    let b = edge.edge_payload_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        assert_eq!(weights, vec![3, 7, 11]);
    }

    #[test]
    fn edge_payloads_survive_unrelated() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        let rail = BucketLabelKey::from_raw(3);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), rail, 2u16)
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &42u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                rail,
                PayloadTestEdge::with_bytes(2, &0u16.to_le_bytes()),
            )
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    let b = edge.edge_payload_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        assert_eq!(weights, vec![42]);
    }

    #[test]
    fn edge_payloads_round_trip_when_edge_and_value_use_overflow_log() {
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=31u32 {
            let weight = u16::try_from(target.saturating_mul(10)).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(33, &320u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(33, &330u16.to_le_bytes()),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.payload_log_len() > 0);
        assert!(bucket.payload_log_head() >= 0);

        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    let b = edge.edge_payload_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        let mut expected: Vec<u16> = (1..=31u32)
            .map(|t| u16::try_from(t.saturating_mul(10)).expect("weight fits u16"))
            .collect();
        expected.extend([320, 330]);
        expected.sort_unstable();
        assert_eq!(weights, expected);
    }

    #[test]
    fn payload_log_full_rebalances_payload_log_only_and_insert_succeeds() {
        let graph = payload_test_graph_with_capacity(1 << 24);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();

        for target in 1..=203u32 {
            let weight = u16::try_from(target).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        let leaf = graph.payload_log_leaf(VertexId::from(0));
        assert!(
            graph.values().payload_log_segment_high_water(leaf) < 170,
            "payload log segment should have been released and reused"
        );
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    let b = edge.edge_payload_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        let expected: Vec<u16> = (1..=203u16).collect();
        assert_eq!(weights, expected);
    }

    #[test]
    fn dense_payload_value_batches_follow_requested_order() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        let mut scratch = LabeledPayloadValueBatchScratch::default();
        let mut asc_slots = Vec::new();
        let mut asc = Vec::new();
        graph
            .visit_out_payload_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    assert!(batch.dense);
                    assert_eq!(batch.byte_width, 2u16);
                    assert_eq!(batch.slot_indices.len() * 2, batch.values.len());
                    asc_slots.extend_from_slice(batch.slot_indices);
                    asc.extend(
                        batch
                            .values
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]])),
                    );
                },
            )
            .unwrap();
        assert_eq!(asc_slots, vec![0, 1, 2]);
        assert_eq!(asc, vec![10, 20, 30]);

        let mut desc_slots = Vec::new();
        let mut desc = Vec::new();
        graph
            .visit_out_payload_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| {
                    assert!(batch.dense);
                    desc_slots.extend_from_slice(batch.slot_indices);
                    desc.extend(
                        batch
                            .values
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]])),
                    );
                },
            )
            .unwrap();
        assert_eq!(desc_slots, vec![2, 1, 0]);
        assert_eq!(desc, vec![30, 20, 10]);
    }

    #[test]
    fn dense_payload_value_batches_match_edge_payload_batches() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        let mut value_scratch = LabeledPayloadValueBatchScratch::default();
        let mut from_values = Vec::new();
        graph
            .visit_out_payload_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut value_scratch,
                |batch| from_values.extend_from_slice(batch.values),
            )
            .unwrap();

        let mut batch_scratch = LabeledEdgePayloadBatchScratch::default();
        let mut from_batches = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut batch_scratch,
                |batch| {
                    assert!(batch.dense);
                    from_batches.extend_from_slice(batch.payload_bytes);
                },
            )
            .unwrap();
        assert_eq!(from_values, from_batches);
    }

    #[test]
    fn hybrid_out_edge_payload_batches_match_span_iter_for_48_overflow_edges() {
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);
        assert!(graph.bucket_slab_prefix_slots(VertexId::from(0), &bucket) > 0);

        let mut from_span = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                from_span.extend_from_slice(edge.edge_payload_bytes());
            })
            .unwrap();

        let mut saw_dense_slab_batch = false;
        let mut from_batches = Vec::new();
        let mut batch_scratch = LabeledEdgePayloadBatchScratch::default();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut batch_scratch,
                |batch| {
                    if batch.dense {
                        saw_dense_slab_batch = true;
                    }
                    from_batches.extend_from_slice(batch.payload_bytes);
                },
            )
            .unwrap();
        assert!(saw_dense_slab_batch);
        assert_eq!(from_span, from_batches);
    }

    #[test]
    fn out_bucket_dense_payload_batch_eligible_matches_dense_vs_overflow_hub() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        assert!(
            graph
                .out_bucket_dense_payload_batch_eligible(VertexId::from(0), road)
                .unwrap()
        );

        let overflow = payload_test_graph_with_capacity(1 << 16);
        overflow.push_vertex(LabeledVertex::default()).unwrap();
        overflow
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            overflow
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        assert!(
            !overflow
                .out_bucket_dense_payload_batch_eligible(VertexId::from(0), road)
                .unwrap()
        );
    }

    #[test]
    fn sparse_payload_value_batches_match_edge_payload_batches() {
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        let mut from_span = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                from_span.extend_from_slice(edge.edge_payload_bytes());
            })
            .unwrap();

        let mut from_values = Vec::new();
        let mut scratch = LabeledPayloadValueBatchScratch::default();
        graph
            .visit_out_payload_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| {
                    from_values.extend_from_slice(batch.values);
                },
            )
            .unwrap();
        assert_eq!(from_span, from_values);
    }

    #[test]
    fn sparse_payload_first_phase_matches_combined_batch_edges() {
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let mut value_scratch = LabeledPayloadValueBatchScratch::default();
        let mut match_slots = Vec::new();
        graph
            .visit_out_payload_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut value_scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    for (idx, slot) in batch.slot_indices.iter().enumerate() {
                        let start = idx * width;
                        let weight =
                            u16::from_le_bytes([batch.values[start], batch.values[start + 1]]);
                        if weight >= 20 {
                            match_slots.push(*slot);
                        }
                    }
                },
            )
            .unwrap();
        assert!(!match_slots.is_empty());

        let mut two_phase = Vec::new();
        graph
            .read_out_edge_slots_for_label(
                VertexId::from(0),
                road,
                &match_slots,
                OutEdgeOrder::Descending,
                |edge| two_phase.push(edge.target),
            )
            .unwrap();

        let mut batch_scratch = LabeledEdgePayloadBatchScratch::default();
        let mut combined = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut batch_scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    for (edge, value) in batch
                        .edges
                        .iter()
                        .zip(batch.payload_bytes.chunks_exact(width))
                    {
                        let weight = u16::from_le_bytes([value[0], value[1]]);
                        if weight >= 20 {
                            combined.push(edge.target);
                        }
                    }
                },
            )
            .unwrap();
        assert_eq!(two_phase, combined);
    }

    #[test]
    fn dense_read_out_edge_slots_follow_requested_order() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        let mut asc = Vec::new();
        graph
            .read_out_edge_slots_for_label(
                VertexId::from(0),
                road,
                &[0, 1, 2],
                OutEdgeOrder::Ascending,
                |edge| asc.push(edge.target),
            )
            .unwrap();
        assert_eq!(asc, vec![1, 2, 3]);

        let mut desc = Vec::new();
        graph
            .read_out_edge_slots_for_label(
                VertexId::from(0),
                road,
                &[0, 1, 2],
                OutEdgeOrder::Descending,
                |edge| desc.push(edge.target),
            )
            .unwrap();
        assert_eq!(desc, vec![3, 2, 1]);

        let mut subset = Vec::new();
        graph
            .read_out_edge_slots_for_label(
                VertexId::from(0),
                road,
                &[2, 0],
                OutEdgeOrder::Descending,
                |edge| subset.push(edge.target),
            )
            .unwrap();
        assert_eq!(subset, vec![3, 1]);
    }

    #[test]
    fn dense_read_out_edge_slots_match_topology_foreach() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        let mut from_foreach = Vec::new();
        graph
            .for_each_edges_for_label_topology_ordered(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                |edge| from_foreach.push((edge.edge_slot_index_raw(), edge.target)),
            )
            .unwrap();

        let slots: Vec<u32> = from_foreach.iter().map(|(slot, _)| *slot).collect();
        let mut from_read = Vec::new();
        graph
            .read_out_edge_slots_for_label(
                VertexId::from(0),
                road,
                &slots,
                OutEdgeOrder::Descending,
                |edge| from_read.push((edge.edge_slot_index_raw(), edge.target)),
            )
            .unwrap();
        assert_eq!(from_read, from_foreach);
    }

    #[test]
    fn payload_first_dense_phase_matches_combined_batch_edges() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        let mut value_scratch = LabeledPayloadValueBatchScratch::default();
        let mut match_slots = Vec::new();
        graph
            .visit_out_payload_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut value_scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    for (idx, slot) in batch.slot_indices.iter().enumerate() {
                        let start = idx * width;
                        let weight =
                            u16::from_le_bytes([batch.values[start], batch.values[start + 1]]);
                        if weight >= 20 {
                            match_slots.push(*slot);
                        }
                    }
                },
            )
            .unwrap();
        assert_eq!(match_slots, vec![2, 1]);

        let mut two_phase = Vec::new();
        graph
            .read_out_edge_slots_for_label(
                VertexId::from(0),
                road,
                &match_slots,
                OutEdgeOrder::Descending,
                |edge| two_phase.push(edge.target),
            )
            .unwrap();
        assert_eq!(two_phase, vec![3, 2]);

        let mut batch_scratch = LabeledEdgePayloadBatchScratch::default();
        let mut combined = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut batch_scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    for (edge, value) in batch
                        .edges
                        .iter()
                        .zip(batch.payload_bytes.chunks_exact(width))
                    {
                        let weight = u16::from_le_bytes([value[0], value[1]]);
                        if weight >= 20 {
                            combined.push(edge.target);
                        }
                    }
                },
            )
            .unwrap();
        assert_eq!(two_phase, combined);
    }

    #[test]
    fn sparse_read_out_edge_slots_resolve_log_backed_indices() {
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let mut from_foreach = Vec::new();
        graph
            .for_each_edges_for_label_topology_ordered(
                VertexId::from(0),
                road,
                OutEdgeOrder::Ascending,
                |edge| from_foreach.push((edge.edge_slot_index_raw(), edge.target)),
            )
            .unwrap();
        let first = from_foreach.first().copied().expect("first edge");
        let last = from_foreach.last().copied().expect("last edge");

        let mut read = Vec::new();
        graph
            .read_out_edge_slots_for_label(
                VertexId::from(0),
                road,
                &[first.0, last.0],
                OutEdgeOrder::Ascending,
                |edge| read.push((edge.edge_slot_index_raw(), edge.target)),
            )
            .unwrap();
        assert_eq!(read, vec![first, last]);
    }

    #[test]
    fn dense_edge_payload_batches_follow_requested_order() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let mut asc = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    assert!(batch.dense);
                    assert_eq!(batch.byte_width, 2u16);
                    asc.extend(
                        batch
                            .payload_bytes
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]])),
                    );
                },
            )
            .unwrap();
        assert_eq!(asc, vec![10, 20, 30]);
        let mut from_iter = Vec::new();
        graph
            .for_each_edges_for_label_ordered(
                VertexId::from(0),
                road,
                OutEdgeOrder::Ascending,
                |edge| {
                    let bytes = edge.edge_payload_bytes();
                    from_iter.push(u16::from_le_bytes([bytes[0], bytes[1]]));
                },
            )
            .unwrap();
        assert_eq!(asc, from_iter);

        let mut desc = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| {
                    assert!(batch.dense);
                    desc.extend(
                        batch
                            .payload_bytes
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]])),
                    );
                },
            )
            .unwrap();
        assert_eq!(desc, vec![30, 20, 10]);
    }

    #[test]
    fn edge_payload_batches_keep_label_widths_separate() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let tiny = BucketLabelKey::from_raw(2);
        let wide = BucketLabelKey::from_raw(3);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), tiny, 1u16)
            .unwrap();
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), wide, 16u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                tiny,
                PayloadTestEdge::with_bytes(1, &[7]),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                wide,
                PayloadTestEdge::with_bytes(2, &[9; 16]),
            )
            .unwrap();

        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let mut tiny_bytes = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                tiny,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    assert_eq!(batch.label_id, tiny);
                    assert_eq!(batch.byte_width, 1u16);
                    tiny_bytes.extend_from_slice(batch.payload_bytes);
                },
            )
            .unwrap();
        assert_eq!(tiny_bytes, vec![7]);

        let mut wide_bytes = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                wide,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    assert_eq!(batch.label_id, wide);
                    assert_eq!(batch.byte_width, 16u16);
                    wide_bytes.extend_from_slice(batch.payload_bytes);
                },
            )
            .unwrap();
        assert_eq!(wide_bytes, vec![9; 16]);
    }

    #[test]
    fn log_backed_edge_payload_batches_match_iterator_values() {
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        let mut from_iter = Vec::new();
        graph
            .for_each_edges_for_label_ordered(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                |edge| from_iter.extend_from_slice(edge.edge_payload_bytes()),
            )
            .unwrap();

        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let mut from_batches = Vec::new();
        graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| from_batches.extend_from_slice(batch.payload_bytes),
            )
            .unwrap();
        assert_eq!(from_batches, from_iter);
    }

    #[test]
    fn valued_default_label_insert_uses_bucket_storage() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let default = graph.default_label();
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), default, 2u16)
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                default,
                PayloadTestEdge::with_bytes(1, &42u16.to_le_bytes()),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(
            !vertex.is_default_edge_labeled(),
            "valued default-label edges need value bucket metadata"
        );
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), default, |edge| {
                if edge.payload_len == 2 {
                    let b = edge.edge_payload_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        assert_eq!(weights, vec![42]);
    }

    #[test]
    fn removing_non_last_payloaded_edge_by_slot_preserves_payload_log_head() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 42u16), (2, 99)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .unwrap()
            .try_with_payload_log_head(0)
            .unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, bucket)
            .unwrap();

        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed edge");

        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert_eq!(bucket.degree(), 1);
        assert_eq!(bucket.payload_log_head(), 0);
    }

    #[test]
    fn valued_insert_reusing_low_tombstone_preserves_existing_values() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }

        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed low slot");
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(4, &40u16.to_le_bytes()),
            )
            .unwrap();

        let mut values = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    values.push((edge.target, {
                        let b = edge.edge_payload_bytes();
                        u16::from_le_bytes([b[0], b[1]])
                    }));
                }
            })
            .unwrap();
        values.sort_unstable();
        assert_eq!(values, vec![(2, 20), (3, 30), (4, 40)]);
    }

    #[test]
    fn edge_payloads_survive_middle_vertex_insert_with_overflow_log() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(1), road, 2u16)
            .unwrap();
        for target in 1..=32u32 {
            let weight = u16::try_from(target).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(1),
                road,
                PayloadTestEdge::with_bytes(2, &2u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(2, &200u16.to_le_bytes()),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);
        assert!(bucket.payload_log_head() >= 0);

        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 && edge.target == 2 {
                    let b = edge.edge_payload_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        assert!(weights.contains(&200), "newest insert weight: {weights:?}");
    }

    #[test]
    fn slab_payload_byte_width_12_round_trips() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        const WIDTH: u16 = 12;
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, WIDTH)
            .unwrap();
        let payload: Vec<u8> = (0..WIDTH).map(|i| (i as u8).wrapping_add(3)).collect();
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &payload),
            )
            .unwrap();
        let mut seen = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == WIDTH {
                    seen.push(edge.edge_payload_bytes().to_vec());
                }
            })
            .unwrap();
        assert_eq!(seen, vec![payload]);
    }

    #[test]
    fn wide_payload_byte_width_12_round_trips_via_overflow_blob_log() {
        const WIDTH: u16 = 12;
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, WIDTH)
            .unwrap();
        let payload: Vec<u8> = (0..WIDTH).map(|i| (i as u8).wrapping_add(9)).collect();
        for target in 1..=31u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &payload),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(33, &payload),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(33, &payload),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(
            bucket.overflow_log_head() >= 0,
            "expected edge overflow log for wide values"
        );
        assert!(
            bucket.payload_log_head() >= 0,
            "expected payload overflow log for 12-byte payloads"
        );

        let mut seen = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == WIDTH {
                    seen.push(edge.edge_payload_bytes().to_vec());
                }
            })
            .unwrap();
        assert_eq!(seen.len(), 33);
        assert!(seen.iter().all(|v| v == &payload));
    }

    #[test]
    fn payload_log_read_failure_is_reported_during_scan() {
        const WIDTH: u16 = 12;
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, WIDTH)
            .unwrap();
        let payload = [7u8; WIDTH as usize];
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &payload),
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.payload_log_head() >= 0);
        graph.values().drop_payload_blob_for_test(
            graph.payload_log_leaf(VertexId::from(0)),
            bucket.payload_log_head() as u32,
        );

        let err = graph
            .for_each_edges_for_label(VertexId::from(0), road, |_| {})
            .expect_err("corrupt payload log must not be converted to zero payload");
        assert!(
            matches!(err, LabeledOperationError::PayloadLogRead(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn payload_log_read_failure_is_reported_by_streaming_apis() {
        const WIDTH: u16 = 12;
        let graph = payload_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, WIDTH)
            .unwrap();
        let payload = [9u8; WIDTH as usize];
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &payload),
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.payload_log_head() >= 0);
        graph.values().drop_payload_blob_for_test(
            graph.payload_log_leaf(VertexId::from(0)),
            bucket.payload_log_head() as u32,
        );

        let mut iter = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(iter.try_advance_by(33).unwrap(), Ok(()));
        assert_eq!(iter.next().transpose().unwrap(), None);

        let err = graph
            .desc_out_edges_iter(VertexId::from(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .expect_err("streaming iterator must report corrupt payload log");
        assert!(
            matches!(err, LabeledOperationError::PayloadLogRead(_)),
            "unexpected iterator error: {err:?}"
        );

        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let err = graph
            .visit_out_edge_payload_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |_| {},
            )
            .expect_err("payload batch traversal must report corrupt payload log");
        assert!(
            matches!(err, LabeledOperationError::PayloadLogRead(_)),
            "unexpected batch error: {err:?}"
        );
    }

    #[test]
    fn find_out_edge_predicate_sees_attached_payload() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &10u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(2, &20u16.to_le_bytes()),
            )
            .unwrap();

        let needle = 20u16.to_le_bytes();
        let found = graph
            .find_out_edge_with_label_by_predicate(VertexId::from(0), |edge| {
                edge.edge_payload_byte_width() == 2 && edge.edge_payload_bytes() == needle
            })
            .unwrap()
            .expect("payload predicate should match");
        assert_eq!(found.0.target, 2);
        assert_eq!(found.1, Some(road));
    }

    #[test]
    fn w4_edge_payloads_round_trip() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 4u16)
            .unwrap();
        for (target, cost) in [(1, 100i32), (2, 200), (3, 300)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_i32(target, cost),
                )
                .unwrap();
        }
        let mut costs = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 4 {
                    costs.push(i32::from_le_bytes(
                        edge.edge_payload_bytes().try_into().unwrap(),
                    ));
                }
            })
            .unwrap();
        costs.sort_unstable();
        assert_eq!(costs, vec![100, 200, 300]);
    }

    #[test]
    fn cannot_change_bucket_payload_width_after_allocation() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &1u16.to_le_bytes()),
            )
            .unwrap();
        assert!(
            graph
                .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 4u16)
                .is_err(),
            "widening an allocated value bucket must fail"
        );
    }

    #[test]
    fn payload_edge_requires_predeclared_bucket_payload_width() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);

        let err = graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &1u16.to_le_bytes()),
            )
            .expect_err("payload edge must not infer bucket payload schema");
        assert!(matches!(
            err,
            LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: 0,
                edge_payload_width: 2
            }
        ));
        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(0)).unwrap(),
            Vec::<BucketLabelKey>::new(),
            "failed payload insert must not create an empty label bucket"
        );
    }

    #[test]
    fn payload_edge_rejected_from_default_bypass_without_promoting_row() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let default = graph.default_label();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                default,
                PayloadTestEdge::with_bytes(1, &[]),
            )
            .unwrap();
        let before = graph.vertices().get(VertexId::from(0));
        assert!(before.is_default_edge_labeled());

        let err = graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                default,
                PayloadTestEdge::with_bytes(2, &2u16.to_le_bytes()),
            )
            .expect_err("payload insert must not promote default bypass row");
        assert!(matches!(
            err,
            LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: 0,
                edge_payload_width: 2
            }
        ));
        let after = graph.vertices().get(VertexId::from(0));
        assert!(after.is_default_edge_labeled());
        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(0)).unwrap(),
            vec![default]
        );
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), default)
                .unwrap(),
            vec![PayloadTestEdge::with_bytes(1, &[])]
        );
    }

    #[test]
    fn non_empty_bucket_rejects_payload_width_changes() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);

        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &[]),
            )
            .unwrap();
        let err = graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .expect_err("non-empty no-payload bucket must not become payloaded");
        assert!(matches!(
            err,
            LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: 0,
                edge_payload_width: 2
            }
        ));

        let valued = BucketLabelKey::from_raw(3);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), valued, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                valued,
                PayloadTestEdge::with_bytes(2, &2u16.to_le_bytes()),
            )
            .unwrap();

        for (edge, expected_width) in [
            (PayloadTestEdge::with_bytes(3, &[]), 0u16),
            (PayloadTestEdge::with_i32(4, 4), 4u16),
        ] {
            let err = graph
                .insert_edge_skip_leaf_cascade(VertexId::from(0), valued, edge)
                .expect_err("payload width must match existing bucket schema");
            assert!(matches!(
                err,
                LabeledOperationError::PayloadByteWidthMismatch {
                    bucket_width: 2,
                    edge_payload_width
                } if edge_payload_width == expected_width
            ));
        }
    }

    #[test]
    fn edge_payloads_survive_rewrite_with_tombstones() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed low slot");

        graph
            .rewrite_vertex_edge_span(VertexId::from(0), None, 1, false, true, None)
            .unwrap();

        let mut values = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    values.push((edge.target, {
                        let b = edge.edge_payload_bytes();
                        u16::from_le_bytes([b[0], b[1]])
                    }));
                }
            })
            .unwrap();
        values.sort_unstable();
        assert_eq!(values, vec![(2, 20), (3, 30)]);
    }

    #[test]
    fn labeled_payload_edge_order_matches_edge_slab_order() {
        let graph = payload_test_graph();
        let src = VertexId::from(0);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(src, road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .remove_edge_at_slot(src, road, 0)
            .unwrap()
            .expect("removed");
        graph
            .rewrite_vertex_edge_span(src, None, 1, false, true, None)
            .unwrap();
        graph.compact_vertex_edge_span(src, 0).unwrap();
        graph
            .test_assert_bucket_payloads_follow_edge_slab_order(src)
            .expect("payload order matches edge slab after rewrite and compact");
    }

    #[test]
    fn edge_payloads_preserved_after_tombstone_delete_and_compact() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20u16), (3, 30u16)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed low slot");
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();

        let mut values = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.payload_len == 2 {
                    values.push((edge.target, {
                        let b = edge.edge_payload_bytes();
                        u16::from_le_bytes([b[0], b[1]])
                    }));
                }
            })
            .unwrap();
        values.sort_unstable();
        assert_eq!(values, vec![(2, 20), (3, 30)]);
    }
}
