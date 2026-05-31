//! Labeled graph `values` implementation.

use crate::{
    VertexId,
    labeled::slot_index::checked_add_slot_index,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
    },
    lara::{edge::EdgeStore, edge_value::EdgeValueStore, operation_error::LaraOperationError},
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
    pub(super) fn bucket_resident_value_bytes(&self, bucket: &LabelBucket) -> u64 {
        crate::labeled::invariants::bucket_resident_value_bytes(bucket)
    }
    pub(super) fn reconcile_vertex_value_allocated_bytes(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<(), LabeledOperationError> {
        let total: u64 = buckets
            .iter()
            .map(|b| self.bucket_resident_value_bytes(b))
            .try_fold(0u64, |acc, bytes| {
                acc.checked_add(bytes)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)
            })?;
        if vertex.value_allocated_bytes() == total {
            debug_assert_eq!(
                vertex.value_allocated_bytes(),
                total,
                "vertex {src:?} value_allocated_bytes must match bucket resident sum"
            );
            return Ok(());
        }
        let updated = vertex
            .try_with_value_allocated_bytes(total)
            .map_err(LabeledOperationError::from)?;
        self.vertices.set(src, &updated);
        debug_assert_eq!(
            self.vertices.get(src).value_allocated_bytes(),
            total,
            "vertex {src:?} value_allocated_bytes must match bucket resident sum after reconcile"
        );
        Ok(())
    }
    pub(super) fn value_log_leaf(&self, src: VertexId) -> u32 {
        u32::from(src) / self.edges.header().segment_size.max(1)
    }
    pub(super) fn vertex_value_spans_need_sync_after_rewrite(
        old_buckets: &[LabelBucket],
        moved: bool,
        old_alloc: u32,
        compact: bool,
    ) -> bool {
        if moved && old_alloc > 0 {
            return true;
        }
        if compact {
            return old_buckets.iter().any(|b| b.is_value_allocated());
        }
        old_buckets.iter().any(|b| {
            b.is_value_allocated() && (b.value_log_head() >= 0 || b.stored_slots != b.degree())
        })
    }
    pub(super) fn read_bucket_values_slab_dense(
        &self,
        bucket: &LabelBucket,
    ) -> Option<Vec<Vec<u8>>> {
        if !bucket.is_value_allocated()
            || bucket.value_byte_width() == 0
            || bucket.value_log_head() >= 0
            || bucket.stored_slots != bucket.degree()
        {
            return None;
        }
        let degree = bucket.degree() as usize;
        let width = usize::from(bucket.value_byte_width());
        let nbytes = degree.checked_mul(width)?;
        let mut raw = vec![0u8; nbytes];
        self.values.read_bytes(bucket.value_offset(), &mut raw);
        Some(
            raw.chunks(width)
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>(),
        )
    }
    pub(super) fn collect_bucket_values_asc_order(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<Vec<Vec<u8>>, LabeledOperationError> {
        if !bucket.is_value_allocated() || bucket.value_byte_width() == 0 {
            return Ok(Vec::new());
        }
        if let Some(dense) = self.read_bucket_values_slab_dense(bucket) {
            return Ok(dense);
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
            out.push(self.read_bucket_value_for_edge(src, bucket, &edge, log_chains.as_ref())?);
        }
        Ok(out)
    }
    pub(super) fn read_bucket_value_for_edge(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        edge: &E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> Result<Vec<u8>, LabeledOperationError> {
        let width = bucket.value_byte_width();
        if width == 0 {
            return Ok(Vec::new());
        }
        let slot_index = edge.edge_slot_index_raw();
        if bucket.value_log_head() < 0 && bucket.overflow_log_head() < 0 {
            let mut buf = vec![0u8; usize::from(width)];
            let offset = bucket
                .value_offset()
                .checked_add(u64::from(slot_index) * u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.values.read_bytes(offset, &mut buf);
            return Ok(buf);
        }

        let leaf = self.value_log_leaf(src);
        if let Some((edge_chain, value_chain)) = log_chains {
            if let Some(bytes) = Self::lookup_bucket_value_in_log_chains(
                &self.edges,
                &self.values,
                leaf,
                width,
                edge,
                edge_chain,
                value_chain,
            )? {
                return Ok(bytes);
            }
        } else {
            let (edge_chain, value_chain) = self.bucket_log_chains(src, bucket);
            if let Some(bytes) = Self::lookup_bucket_value_in_log_chains(
                &self.edges,
                &self.values,
                leaf,
                width,
                edge,
                &edge_chain,
                &value_chain,
            )? {
                return Ok(bytes);
            }
        }

        let mut buf = vec![0u8; usize::from(width)];
        let offset = bucket
            .value_offset()
            .checked_add(u64::from(slot_index) * u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.values.read_bytes(offset, &mut buf);
        Ok(buf)
    }
    pub(super) fn lookup_bucket_value_in_log_chains(
        edges: &EdgeStore<E, M>,
        values: &EdgeValueStore<M>,
        leaf: u32,
        width: u16,
        edge: &E,
        edge_chain: &[u32],
        value_chain: &[u32],
    ) -> Result<Option<Vec<u8>>, LabeledOperationError> {
        debug_assert_eq!(
            edge_chain.len(),
            value_chain.len(),
            "edge/value overflow log chains must have equal length at lookup time"
        );
        let slot_index = edge.edge_slot_index_raw();
        for (&entry_idx, &value_idx) in edge_chain.iter().zip(value_chain.iter()) {
            if entry_idx != slot_index {
                continue;
            }
            let logged = edges.decode_overflow_log_edge_at(leaf, entry_idx);
            if logged.neighbor_vid() == edge.neighbor_vid() {
                let mut buf = vec![0u8; usize::from(width)];
                values.read_value_log_entry(leaf, value_idx, width, &mut buf)?;
                return Ok(Some(buf));
            }
        }
        Ok(None)
    }
    pub(super) fn write_edge_value_to_log(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        entry_idx: i32,
        edge: &E,
    ) -> Result<LabelBucket, LabeledOperationError> {
        let width = bucket.value_byte_width();
        if width == 0 {
            return Ok(*bucket);
        }
        let src_i32 = i32::try_from(u32::from(src))
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        self.values
            .write_value_log_entry(
                self.value_log_leaf(src),
                u32::try_from(entry_idx)
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                bucket.value_log_head(),
                src_i32,
                width,
                edge.edge_value_bytes(),
            )
            .map_err(LabeledOperationError::from)?;
        bucket
            .try_with_value_log_head(entry_idx)
            .map_err(LabeledOperationError::from)
    }
    pub(super) fn release_bucket_value_span(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Result<(), LabeledOperationError> {
        let len = self.bucket_resident_value_bytes(bucket);
        if len == 0 {
            return Ok(());
        }
        self.values
            .retire_byte_span(bucket.value_offset(), len)
            .map_err(LabeledOperationError::from)?;
        let vertex = self.vertices.get(src);
        let new_alloc = vertex.value_allocated_bytes().saturating_sub(len);
        let updated = vertex
            .try_with_value_allocated_bytes(new_alloc)
            .map_err(LabeledOperationError::from)?;
        self.vertices.set(src, &updated);
        Ok(())
    }
    pub(super) fn read_bucket_values_in_edge_slot_order(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<Vec<Vec<u8>>, LabeledOperationError> {
        self.collect_bucket_values_asc_order(src, vertex, bucket_index, bucket)
    }
    pub(super) fn ensure_bucket_value_byte_width_on_slot(
        &self,
        _src: VertexId,
        _bucket_slot: u64,
        bucket: LabelBucket,
        value_byte_width: u16,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.value_byte_width() == value_byte_width {
            return Ok(bucket);
        }
        if bucket.is_value_allocated() && value_byte_width != 0 {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        Ok(bucket.with_value_byte_width(value_byte_width))
    }
    pub fn ensure_label_bucket_value_byte_width(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        value_byte_width: u16,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let (bucket_slot, bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let bucket = self.ensure_bucket_value_byte_width_on_slot(
            src,
            bucket_slot,
            bucket,
            value_byte_width,
        )?;
        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        Ok(())
    }
    pub(super) fn ensure_bucket_value_span(
        &self,
        src: VertexId,
        bucket_slot: u64,
        mut bucket: LabelBucket,
        prev_stored_slots: u32,
    ) -> Result<LabelBucket, LabeledOperationError> {
        let width = bucket.value_byte_width();
        let needed_slots = bucket.stored_slots.max(bucket.degree);
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
        let old_offset = bucket.value_offset();
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
                .with_value_offset(offset)
                .try_with_value_log_head(-1)
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
            bucket = bucket.with_value_offset(new_offset);
            alloc_delta = extra;
            debug_assert_eq!(bucket.value_offset(), new_offset);
        }

        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;

        if alloc_delta > 0 {
            let vertex = self.vertices.get(src);
            let new_alloc = vertex
                .value_allocated_bytes()
                .checked_add(alloc_delta)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let updated = vertex
                .try_with_value_allocated_bytes(new_alloc)
                .map_err(LabeledOperationError::from)?;
            self.vertices.set(src, &updated);
        }
        if bucket.is_value_allocated() {
            let vertex = self.vertices.get(src);
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            self.reconcile_vertex_value_allocated_bytes(src, &vertex, &buckets)?;
        }
        Ok(bucket)
    }
    /// Updates the edge-value payload for one live edge at `slot_index` inside `label_id`.
    pub fn update_edge_value_at_slot(
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
            if edge.edge_value_byte_width() != 0 {
                return Ok(false);
            }
            return Ok(true);
        }
        let (slot, mut bucket) = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => (slot, bucket),
            BucketSearch::Missing { .. } => return Ok(false),
        };
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        if bucket.overflow_log_head() >= 0 {
            bucket = self.ensure_label_bucket_folded_to_slab(src, bucket_index, slot, bucket)?;
        }
        if slot_index >= bucket.stored_slots {
            return Ok(false);
        }
        let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let current = self.edges.read_slot(edge_slot);
        if current.is_deleted_slot() || current.is_tombstone_edge() {
            return Ok(false);
        }
        if edge.edge_value_byte_width() != 0 {
            self.write_edge_value_at_slot(&bucket, slot_index, &edge)?;
        }
        self.buckets.write_label_bucket_slot(slot, bucket)?;
        self.invalidate_bucket_lookup_for_label(src, label_id);
        Ok(true)
    }

    pub(super) fn write_edge_value_at_slot(
        &self,
        bucket: &LabelBucket,
        slot_index: u32,
        edge: &E,
    ) -> Result<(), LabeledOperationError> {
        let width = bucket.value_byte_width();
        if width == 0 {
            return Ok(());
        }
        let value_width = edge.edge_value_byte_width();
        if value_width == 0 {
            return Ok(());
        }
        if value_width != width {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let offset = bucket
            .value_offset()
            .checked_add(u64::from(slot_index) * u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.values
            .write_value_slot(offset, width, edge.edge_value_bytes())
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }
    pub(super) fn attach_edge_value(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        bucket: LabelBucket,
        slot_index: u32,
        edge: E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> E {
        if !bucket.is_value_allocated() {
            return edge;
        }
        let width = bucket.value_byte_width();
        let edge = edge.with_slot_index(slot_index);
        let buf = self
            .read_bucket_value_for_edge(src, &bucket, &edge, log_chains)
            .unwrap_or_else(|_| vec![0u8; usize::from(width)]);
        edge.with_stored_value_bytes(width, &buf)
    }
    pub(super) fn bucket_value_log_chains_opt(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Option<(Vec<u32>, Vec<u32>)> {
        if bucket.is_value_allocated()
            && (bucket.overflow_log_head() >= 0 || bucket.value_log_head() >= 0)
        {
            Some(self.bucket_log_chains(src, bucket))
        } else {
            None
        }
    }
    pub(super) fn labeled_edge_with_value(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        slot_index: u32,
        edge: E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> E {
        self.attach_edge_value(
            src,
            vertex,
            bucket_index,
            bucket,
            slot_index,
            edge,
            log_chains,
        )
    }
    pub(super) fn collect_bucket_values_before_edge_rewrite(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<Vec<Vec<Vec<u8>>>, LabeledOperationError> {
        let mut saved = Vec::with_capacity(buckets.len());
        for (index, bucket) in buckets.iter().enumerate() {
            let values = match self.read_bucket_values_slab_dense(bucket) {
                Some(v) => v,
                None => {
                    self.read_bucket_values_in_edge_slot_order(src, vertex, index as u32, bucket)?
                }
            };
            if bucket.value_byte_width() > 0 {
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

    pub(super) fn sync_vertex_value_spans_after_edge_rewrite(
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
            self.release_bucket_value_span(src, old)?;
        }
        for (index, new_bucket) in new_buckets.iter().enumerate() {
            if new_bucket.value_byte_width() == 0 {
                continue;
            }
            let live = usize::try_from(new_bucket.degree()).unwrap_or(usize::MAX);
            debug_assert_eq!(
                saved[index].len(),
                live,
                "sync after rewrite: saved value count must match new live degree (bucket {index})"
            );
            let slot = Self::labeled_vertex_bucket_slot(&vertex, index as u32)?;
            let mut bucket = new_bucket.with_value_offset(0);
            bucket = self.ensure_bucket_value_span(src, slot, bucket, 0)?;
            let width = bucket.value_byte_width();
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
                    .write_bytes(bucket.value_offset(), &flat)
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
        if vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let has_live_value_span = buckets
            .iter()
            .any(|b| b.is_value_allocated() && self.bucket_resident_value_bytes(b) > 0);
        if has_live_value_span {
            return self.reconcile_vertex_value_allocated_bytes(src, &vertex, &buckets);
        }
        if vertex.value_allocated_bytes() > 0 {
            return Ok(());
        }
        if buckets.iter().any(|b| b.is_value_allocated()) {
            if self.vertex_label_buckets_have_overflow(&vertex)? {
                self.reclaim_vertex_overflow_buckets(src)?;
            }
            self.compact_vertex_edge_span(src, 0)?;
            let vertex = self.vertices.get(src);
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            let total_live = buckets.iter().try_fold(0u32, |acc, bucket| {
                acc.checked_add(bucket.degree())
                    .ok_or(LaraOperationError::RowDegreeOverflow)
            })?;
            if vertex.stored_slots.saturating_sub(total_live) < 2 {
                self.rewrite_vertex_edge_span(src, None, 1, false, true)?;
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
    fn edge_values_round_trip_via_unchecked_label_iteration() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 1))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(2, 100),
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        if let BucketSearch::Found { bucket, .. } =
            graph.find_bucket(VertexId::from(0), &vertex, road).unwrap()
        {
            let mut raw = vec![0u8; 4];
            graph.values().read_bytes(bucket.value_offset(), &mut raw);
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
            .filter(|e| e.value_len == 2)
            .map(|e| {
                let b = e.edge_value_bytes();
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
    fn edge_values_survive_middle_vertex_insert() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 1))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(1), road, ValuedTestEdge::with_u16(2, 1))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(2, 100),
            )
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    let b = edge.edge_value_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 100]);
    }
    #[test]
    fn edge_values_preserved() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1u32, 3u16), (2, 7), (3, 11)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    let b = edge.edge_value_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        assert_eq!(weights, vec![3, 7, 11]);
    }
    #[test]
    fn edge_values_survive_unrelated() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        let rail = BucketLabelKey::from_raw(3);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 42))
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), rail, ValuedTestEdge::with_u16(2, 0))
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    let b = edge.edge_value_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        assert_eq!(weights, vec![42]);
    }
    #[test]
    fn edge_values_round_trip_when_edge_and_value_use_overflow_log() {
        let graph = valued_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=31u32 {
            let weight = u16::try_from(target.saturating_mul(10)).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(33, 320),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(33, 330),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);
        assert!(bucket.value_log_head() >= 0);

        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    let b = edge.edge_value_bytes();
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
    fn dense_edge_value_batches_follow_requested_order() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10u16), (2, 20), (3, 30)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }

        let mut scratch = LabeledEdgeValueBatchScratch::default();
        let mut asc = Vec::new();
        graph
            .visit_out_edge_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    assert!(batch.dense);
                    assert_eq!(batch.byte_width, 2u16);
                    asc.extend(
                        batch
                            .value_bytes
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]])),
                    );
                },
            )
            .unwrap();
        assert_eq!(asc, vec![10, 20, 30]);

        let mut desc = Vec::new();
        graph
            .visit_out_edge_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| {
                    assert!(batch.dense);
                    desc.extend(
                        batch
                            .value_bytes
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]])),
                    );
                },
            )
            .unwrap();
        assert_eq!(desc, vec![30, 20, 10]);
    }

    #[test]
    fn edge_value_batches_keep_label_widths_separate() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let tiny = BucketLabelKey::from_raw(2);
        let wide = BucketLabelKey::from_raw(3);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), tiny, 1u16)
            .unwrap();
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), wide, 16u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                tiny,
                ValuedTestEdge::with_bytes(1, &[7]),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                wide,
                ValuedTestEdge::with_bytes(2, &[9; 16]),
            )
            .unwrap();

        let mut scratch = LabeledEdgeValueBatchScratch::default();
        let mut tiny_bytes = Vec::new();
        graph
            .visit_out_edge_value_batches_for_label(
                VertexId::from(0),
                tiny,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    assert_eq!(batch.label_id, tiny);
                    assert_eq!(batch.byte_width, 1u16);
                    tiny_bytes.extend_from_slice(batch.value_bytes);
                },
            )
            .unwrap();
        assert_eq!(tiny_bytes, vec![7]);

        let mut wide_bytes = Vec::new();
        graph
            .visit_out_edge_value_batches_for_label(
                VertexId::from(0),
                wide,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    assert_eq!(batch.label_id, wide);
                    assert_eq!(batch.byte_width, 16u16);
                    wide_bytes.extend_from_slice(batch.value_bytes);
                },
            )
            .unwrap();
        assert_eq!(wide_bytes, vec![9; 16]);
    }

    #[test]
    fn log_backed_edge_value_batches_match_iterator_values() {
        let graph = valued_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, target as u16),
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
                |edge| from_iter.extend_from_slice(edge.edge_value_bytes()),
            )
            .unwrap();

        let mut scratch = LabeledEdgeValueBatchScratch::default();
        let mut from_batches = Vec::new();
        graph
            .visit_out_edge_value_batches_for_label(
                VertexId::from(0),
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| {
                    assert!(!batch.dense);
                    from_batches.extend_from_slice(batch.value_bytes);
                },
            )
            .unwrap();
        assert_eq!(from_batches, from_iter);
    }
    #[test]
    fn valued_default_label_insert_uses_bucket_storage() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let default = graph.default_label();
        graph
            .insert_edge(VertexId::from(0), default, ValuedTestEdge::with_u16(1, 42))
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(
            !vertex.is_default_edge_labeled(),
            "valued default-label edges need value bucket metadata"
        );
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), default, |edge| {
                if edge.value_len == 2 {
                    let b = edge.edge_value_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        assert_eq!(weights, vec![42]);
    }
    #[test]
    fn removing_non_last_valued_edge_by_slot_preserves_value_log_head() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 42), (2, 99)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .unwrap()
            .try_with_value_log_head(0)
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
        assert_eq!(bucket.value_log_head(), 0);
    }
    #[test]
    fn valued_insert_reusing_low_tombstone_preserves_existing_values() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10), (2, 20), (3, 30)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }

        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed low slot");
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(4, 40))
            .unwrap();

        let mut values = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    values.push((edge.target, {
                        let b = edge.edge_value_bytes();
                        u16::from_le_bytes([b[0], b[1]])
                    }));
                }
            })
            .unwrap();
        values.sort_unstable();
        assert_eq!(values, vec![(2, 20), (3, 30), (4, 40)]);
    }
    #[test]
    fn edge_values_survive_middle_vertex_insert_with_overflow_log() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=32u32 {
            let weight = u16::try_from(target).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(1), road, ValuedTestEdge::with_u16(2, 2))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(2, 200),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);
        assert!(bucket.value_log_head() >= 0);

        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 && edge.target == 2 {
                    let b = edge.edge_value_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        assert!(weights.contains(&200), "newest insert weight: {weights:?}");
    }

    #[test]
    fn slab_value_byte_width_12_round_trips() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        const WIDTH: u16 = 12;
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, WIDTH)
            .unwrap();
        let payload: Vec<u8> = (0..WIDTH).map(|i| (i as u8).wrapping_add(3)).collect();
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_bytes(1, &payload),
            )
            .unwrap();
        let mut seen = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == WIDTH {
                    seen.push(edge.edge_value_bytes().to_vec());
                }
            })
            .unwrap();
        assert_eq!(seen, vec![payload]);
    }

    #[test]
    fn wide_value_byte_width_12_round_trips_via_overflow_blob_log() {
        const WIDTH: u16 = 12;
        let graph = valued_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, WIDTH)
            .unwrap();
        let payload: Vec<u8> = (0..WIDTH).map(|i| (i as u8).wrapping_add(9)).collect();
        for target in 1..=31u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_bytes(target, &payload),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_bytes(33, &payload),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_bytes(33, &payload),
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
            bucket.value_log_head() >= 0,
            "expected value overflow log for 12-byte payloads"
        );

        let mut seen = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == WIDTH {
                    seen.push(edge.edge_value_bytes().to_vec());
                }
            })
            .unwrap();
        assert_eq!(seen.len(), 33);
        assert!(seen.iter().all(|v| v == &payload));
    }

    #[test]
    fn w4_edge_values_round_trip() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 4u16)
            .unwrap();
        for (target, cost) in [(1, 100i32), (2, 200), (3, 300)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_i32(target, cost),
                )
                .unwrap();
        }
        let mut costs = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 4 {
                    costs.push(i32::from_le_bytes(
                        edge.edge_value_bytes().try_into().unwrap(),
                    ));
                }
            })
            .unwrap();
        costs.sort_unstable();
        assert_eq!(costs, vec![100, 200, 300]);
    }

    #[test]
    fn cannot_change_bucket_value_width_after_allocation() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 1))
            .unwrap();
        assert!(
            graph
                .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 4u16)
                .is_err(),
            "widening an allocated value bucket must fail"
        );
    }

    #[test]
    fn edge_values_survive_rewrite_with_tombstones() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10), (2, 20), (3, 30)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed low slot");

        graph
            .rewrite_vertex_edge_span(VertexId::from(0), None, 1, false, true)
            .unwrap();

        let mut values = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    values.push((edge.target, {
                        let b = edge.edge_value_bytes();
                        u16::from_le_bytes([b[0], b[1]])
                    }));
                }
            })
            .unwrap();
        values.sort_unstable();
        assert_eq!(values, vec![(2, 20), (3, 30)]);
    }

    #[test]
    fn edge_values_preserved_after_tombstone_delete_and_compact() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for (target, weight) in [(1, 10), (2, 20), (3, 30)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
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
                if edge.value_len == 2 {
                    values.push((edge.target, {
                        let b = edge.edge_value_bytes();
                        u16::from_le_bytes([b[0], b[1]])
                    }));
                }
            })
            .unwrap();
        values.sort_unstable();
        assert_eq!(values, vec![(2, 20), (3, 30)]);
    }
}
