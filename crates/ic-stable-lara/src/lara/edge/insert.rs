//! EdgeStore `insert` implementation.

use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    VertexId,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex, CsrVertexTombstoneScan},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;

use super::scan_iter::{OutEdgeSlabIter, leaf_segment};
use super::{DeleteTarget, EdgeStore, InsertLocation, decode_delete_target};
impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub(super) fn collect_out_edge_refs_slot_order<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<Vec<(DeleteTarget, E)>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        // Tombstone rows may still hold slab/log material while incremental
        // `DeleteVertex` maintenance runs; only fully evacuated rows reject reads.
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(Vec::new());
        }
        if v.log_head() < 0 {
            let stored = v.stored_degree() as usize;
            let live = v.degree() as usize;
            let base = v.base_slot_start();
            if live == 0 {
                return Ok(Vec::new());
            }
            let nbytes = stored
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
            let mut out = Vec::with_capacity(live);
            for (offset, chunk) in raw.chunks_exact(E::BYTES).enumerate() {
                let edge = E::read_from(chunk);
                if edge.is_deleted_slot() {
                    continue;
                }
                let slot_index = offset as u32;
                out.push((
                    DeleteTarget::Slab(slot_index),
                    edge.with_slot_index(slot_index),
                ));
            }
            debug_assert_eq!(
                out.len(),
                live,
                "slab row must have exactly `degree` live edges among stored slots"
            );
            return Ok(out);
        }

        let edge_layout = self.edge_layout();
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let slab_count = on_slab.min(v.stored_degree()) as usize;
        let mut out = Vec::with_capacity(v.degree() as usize);
        for i in 0..slab_count {
            let slot = v
                .base_slot_start()
                .checked_add(i as u64)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.read_slot(slot);
            if !edge.is_deleted_slot() {
                let slot_index = i as u32;
                out.push((
                    DeleteTarget::Slab(slot_index),
                    edge.with_slot_index(slot_index),
                ));
            }
        }
        if v.log_head() < 0 {
            return Ok(out);
        }

        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let log_h = self.log.header();

        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(&log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let mut entries = Vec::new();
        let mut log_i = v.log_head();
        let mut steps = 0u32;
        while log_i >= 0 {
            if steps >= log_h.max_log_entries {
                return Err(LaraOperationError::LogChainShort);
            }
            let (prev, src, edge) =
                self.read_log_edge_from_table_or_store(&log_h, leaf, log_i as u32, log_table);
            entries.push((log_i as u32, src, edge));
            log_i = prev;
            steps = steps
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        entries.reverse();

        for (log_idx, src, edge) in entries {
            if let Some(target) = decode_delete_target(src) {
                if let Some(index) = out.iter().position(|(candidate, _)| *candidate == target) {
                    out.remove(index);
                }
            } else {
                out.push((DeleteTarget::Log(log_idx), edge));
            }
        }
        debug_assert_eq!(
            out.len(),
            v.degree() as usize,
            "logical log replay must yield exactly `degree` live edges"
        );
        if out.len() != v.degree() as usize {
            // The log chain may be truncated/corrupt; preserve the old error shape rather than
            // silently returning a count that violates the vertex row.
            return Err(LaraOperationError::LogChainShort);
        }
        Ok(out)
    }
    pub(crate) fn insert_edge<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
        edge: E,
    ) -> Result<InsertLocation, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        if V::record_is_vertex_tombstone(&v) {
            return Err(LaraOperationError::VertexDeleted);
        }
        let log_owner = vertices.log_leaf_vertex(vid);

        let _next_degree = v
            .degree()
            .checked_add(1)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let next_num_edges = edge_layout
            .num_edges
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let loc = v
            .base_slot_start()
            .checked_add(u64::from(v.stored_degree()))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let location = if self.have_space_on_slab(vertices, v_ord, &v, loc, &edge_layout) {
            let write_end = loc
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if write_end > self.header().elem_capacity {
                self.set_elem_capacity(write_end)
                    .map_err(LaraOperationError::ResizeFailed)?;
            }
            self.write_slot(loc, edge)
                .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
            let grown = v
                .try_grow_packed_slab_by_one()
                .map_err(|()| LaraOperationError::RowDegreeOverflow)?;
            vertices.set(vid, &grown);
            InsertLocation::Slab(v.stored_degree())
        } else {
            self.insert_into_log_with_layout(
                &edge_layout,
                vertices,
                vid,
                log_owner,
                v,
                _next_degree,
                edge,
            )?;
            InsertLocation::Log
        };
        self.set_num_edges(next_num_edges);
        self.bump_counts_leaf_with_layout(&edge_layout, log_owner, 1, 0)?;
        Ok(location)
    }
    pub(crate) fn remove_edge_slab_tombstone_matching<V, A, F>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let log_owner = vertices.log_leaf_vertex(vid);
        if v.log_head() >= 0 {
            let removed = self
                .collect_out_edge_refs_slot_order(vertices, vid)?
                .into_iter()
                .find(|(_, edge)| matches(edge));
            let Some((target, removed)) = removed else {
                return Ok(None);
            };
            self.insert_delete_into_log_with_layout(
                &edge_layout,
                vertices,
                vid,
                v,
                target,
                removed,
            )?;
            let next_global = edge_layout
                .num_edges
                .checked_sub(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.set_num_edges(next_global);
            self.bump_counts_leaf_with_layout(&edge_layout, log_owner, -1, 0)?;
            return Ok(Some(removed));
        }
        let live = v.degree();
        if live == 0 {
            return Ok(None);
        }

        let base = v.base_slot_start();
        let stored = v.stored_degree();
        let mut found_index: Option<u32> = None;
        for i in 0..stored {
            let slot = base
                .checked_add(u64::from(i))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.read_slot(slot);
            if edge.is_tombstone_edge() {
                continue;
            }
            if matches(&edge) {
                found_index = Some(i);
                break;
            }
        }
        let Some(local_index) = found_index else {
            return Ok(None);
        };

        let rm_slot = base
            .checked_add(u64::from(local_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.read_slot(rm_slot);
        self.write_slot(rm_slot, E::tombstone_edge())
            .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
        vertices.set(vid, &v.after_slab_tombstone_delete());
        let next_global = edge_layout
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.set_num_edges(next_global);
        self.bump_counts_leaf_with_layout(&edge_layout, log_owner, -1, 0)?;
        Ok(Some(removed))
    }
    pub(crate) fn row_edge_at_slab<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
        offset: u32,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        if v.log_head() >= 0 {
            return Err(LaraOperationError::RowEdgeReadRequiresSlabOnlyRow);
        }
        if offset >= v.degree() {
            return Ok(None);
        }
        let mut seen = 0u32;
        for stored_offset in 0..v.stored_degree() {
            let slot = v
                .base_slot_start()
                .checked_add(u64::from(stored_offset))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.read_slot(slot);
            if edge.is_deleted_slot() {
                continue;
            }
            if seen == offset {
                return Ok(Some(edge));
            }
            seen = seen
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(None)
    }
    pub(crate) fn find_first_out_edge_slot_matching<V, A, F>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut matches: F,
    ) -> Result<Option<(u32, E)>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
        F: FnMut(&E) -> bool,
    {
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(None);
        }
        if v.log_head() >= 0 {
            return Ok(None);
        }
        let mut it =
            OutEdgeSlabIter::try_new(self, v.base_slot_start(), v.stored_degree(), v.degree())?;
        while let Some((slot, edge)) = it.next_live_edge_with_slot() {
            if matches(&edge) {
                return Ok(Some((slot, edge)));
            }
        }
        Ok(None)
    }
    pub(crate) fn remove_edge_at_slab_slot<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
        slot_index: u32,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        E: CsrEdgeTombstone,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        if v.log_head() >= 0 || slot_index >= v.stored_degree() {
            return Ok(None);
        }
        let rm_slot = v
            .base_slot_start()
            .checked_add(u64::from(slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.read_slot(rm_slot);
        if removed.is_deleted_slot() {
            return Ok(None);
        }
        self.write_slot(rm_slot, E::tombstone_edge())
            .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
        vertices.set(vid, &v.after_slab_tombstone_delete());
        let next_global = edge_layout
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.set_num_edges(next_global);
        self.bump_counts_leaf_with_layout(&edge_layout, vertices.log_leaf_vertex(vid), -1, 0)?;
        Ok(Some(removed))
    }
    pub(crate) fn clear_row_slab<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<u32, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let log_owner = vertices.log_leaf_vertex(vid);
        if v.log_head() >= 0 {
            return Err(LaraOperationError::ClearRowRequiresSlabOnlyRow);
        }
        let removed = v.degree();
        if removed == 0 {
            return Ok(0);
        }
        vertices.set(vid, &v.with_degree(0).with_log_head(-1));
        let next_global = edge_layout
            .num_edges
            .checked_sub(u64::from(removed))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.set_num_edges(next_global);
        self.bump_counts_leaf_with_layout(&edge_layout, log_owner, -i64::from(removed), 0)?;
        Ok(removed)
    }
}
