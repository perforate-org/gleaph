//! EdgeStore `log_mut` implementation.

use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    SegmentId, VertexId,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use ic_stable_structures::Memory;

use super::log::HeaderV1 as LogHeaderV1;
use super::scan_iter::leaf_segment;
use super::{DeleteTarget, EdgeLayout, EdgeStore, INLINE_EDGE_BYTES};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub(crate) fn overflow_log_chain_len(&self, leaf: u32, head: i32) -> u32 {
        if head < 0 {
            return 0;
        }
        let h = self.log.header();
        let max_entries = h.max_log_entries;
        let scratch_len = E::BYTES.max(1);
        let mut cur = head;
        let mut len = 0u32;
        while cur >= 0 {
            // Defensive budget guard: a valid per-segment overflow chain has at most
            // `max_log_entries` entries. A longer walk means a corrupt `prev` pointer
            // (cycle or dangling link); stop instead of looping unbounded.
            if len >= max_entries {
                break;
            }
            len = len.saturating_add(1);
            if E::BYTES <= INLINE_EDGE_BYTES {
                let mut scratch = [0u8; INLINE_EDGE_BYTES];
                let prev = self.log.read_entry_with_header(
                    &h,
                    leaf,
                    cur as u32,
                    &mut scratch[..scratch_len],
                );
                cur = prev;
            } else {
                let mut scratch = vec![0u8; E::BYTES];
                let prev = self
                    .log
                    .read_entry_with_header(&h, leaf, cur as u32, &mut scratch);
                cur = prev;
            }
        }
        len
    }

    pub(crate) fn overflow_log_chain_asc_indices(&self, leaf: u32, head: i32) -> Vec<u32> {
        if head < 0 {
            return Vec::new();
        }
        let mut chain = Vec::new();
        let mut cur = head;
        let h = self.log.header();
        let max_entries = h.max_log_entries as usize;
        let scratch_len = E::BYTES.max(1);
        while cur >= 0 {
            // Defensive budget guard (mirrors the prefetch_* walks): a valid per-segment
            // overflow chain visits each of the segment's `<= max_log_entries` entries at
            // most once. A longer walk means a corrupt `prev` pointer (cycle or dangling
            // link); stop instead of looping unbounded and growing `chain` without limit.
            if chain.len() >= max_entries {
                break;
            }
            chain.push(cur as u32);
            if E::BYTES <= INLINE_EDGE_BYTES {
                let mut scratch = [0u8; INLINE_EDGE_BYTES];
                let prev = self.log.read_entry_with_header(
                    &h,
                    leaf,
                    cur as u32,
                    &mut scratch[..scratch_len],
                );
                cur = prev;
            } else {
                let mut scratch = vec![0u8; E::BYTES];
                let prev = self
                    .log
                    .read_entry_with_header(&h, leaf, cur as u32, &mut scratch);
                cur = prev;
            }
        }
        chain.reverse();
        chain
    }

    pub(crate) fn decode_overflow_log_edge_at(&self, leaf: u32, entry_idx: u32) -> E {
        let h = self.log.header();
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            self.log
                .read_entry_with_header(&h, leaf, entry_idx, &mut payload[..E::BYTES]);
            E::read_from(&payload[..E::BYTES]).with_slot_index(entry_idx)
        } else {
            let mut payload = vec![0u8; E::BYTES];
            self.log
                .read_entry_with_header(&h, leaf, entry_idx, &mut payload);
            E::read_from(&payload).with_slot_index(entry_idx)
        }
    }

    pub(crate) fn read_overflow_log_entry(&self, leaf: u32, entry_idx: u32) -> (i32, E) {
        let h = self.log.header();
        self.read_log_edge_from_table_or_store(&h, leaf, entry_idx, None)
    }

    pub(super) fn prefetch_descending_log_entries(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<Option<E>>, Vec<u32>), LaraOperationError> {
        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let deleted_slab_offsets: Vec<u32> = Vec::new();
        let mut log_entries: Vec<Option<E>> = Vec::new();
        let mut log_i = log_head;
        let mut budget = log_h.max_log_entries;
        while budget > 0 {
            budget -= 1;
            if log_i < 0 {
                return Ok((log_entries, deleted_slab_offsets));
            }
            let log_idx = log_i as u32;
            let (prev, edge) =
                self.read_log_edge_from_table_or_store(log_h, leaf, log_idx, log_table);
            log_i = prev;
            if edge.is_deleted_slot() {
                log_entries.push(None);
                continue;
            }
            log_entries.push(Some(edge));
        }
        if log_i >= 0 {
            return Err(LaraOperationError::LogChainShort);
        }
        Ok((log_entries, deleted_slab_offsets))
    }

    pub(super) fn read_log_edge_from_table_or_store(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_idx: u32,
        table: Option<&[u8]>,
    ) -> (i32, E) {
        if let Some(buf) = table {
            let stride = log_h.stride as usize;
            if stride > 0 {
                let off = log_idx as usize * stride;
                if off + stride <= buf.len() && off + 4 + E::BYTES <= buf.len() {
                    let prev = i32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                    let edge = E::read_from(&buf[off + 4..off + 4 + E::BYTES]);
                    return (prev, edge);
                }
            }
        }
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            let prev = self
                .log
                .read_entry_with_header(log_h, leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, E::read_from(&buf[..E::BYTES]))
        } else {
            let mut buf = vec![0u8; E::BYTES];
            let prev = self
                .log
                .read_entry_with_header(log_h, leaf, log_idx, &mut buf);
            (prev, E::read_from(&buf))
        }
    }

    /// Overflow-log replay for descending scan (`None` = dead placeholder slot).
    pub(super) fn prefetch_descending_log_replay_tags(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<Option<u32>>, Vec<u32>, Vec<u8>), LaraOperationError> {
        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let deleted_slab_offsets: Vec<u32> = Vec::new();
        let mut replay_entries: Vec<Option<u32>> = Vec::new();
        let mut log_i = log_head;
        let mut budget = log_h.max_log_entries;
        while budget > 0 {
            budget -= 1;
            if log_i < 0 {
                return Ok((replay_entries, deleted_slab_offsets, log_table_buf));
            }
            let log_idx = log_i as u32;
            let (prev, edge) =
                self.read_log_edge_from_table_or_store(log_h, leaf, log_idx, log_table);
            log_i = prev;
            if edge.is_deleted_slot() {
                replay_entries.push(None);
                continue;
            }
            replay_entries.push(Some(log_idx));
        }
        if log_i >= 0 {
            return Err(LaraOperationError::LogChainShort);
        }
        Ok((replay_entries, deleted_slab_offsets, log_table_buf))
    }

    /// Overflow-log inserted-edge replay for ascending scan (`None` consumes a slot).
    pub(super) fn prefetch_ascending_log_inserted_tags(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<Option<u32>>, Vec<u32>, Vec<u8>), LaraOperationError> {
        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let mut entries = Vec::new();
        let mut log_i = log_head;
        let mut steps = 0u32;
        while log_i >= 0 {
            if steps >= log_h.max_log_entries {
                return Err(LaraOperationError::LogChainShort);
            }
            let (prev, edge) =
                self.read_log_edge_from_table_or_store(log_h, leaf, log_i as u32, log_table);
            entries.push((log_i as u32, edge));
            log_i = prev;
            steps = steps
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        entries.reverse();

        let deleted_slab_offsets: Vec<u32> = Vec::new();
        let inserted: Vec<Option<u32>> = entries
            .into_iter()
            .map(|(log_idx, edge)| {
                if edge.is_deleted_slot() {
                    None
                } else {
                    Some(log_idx)
                }
            })
            .collect();

        Ok((inserted, deleted_slab_offsets, log_table_buf))
    }

    pub(crate) fn decode_overflow_log_edge_from_table(
        &self,
        leaf: u32,
        log_idx: u32,
        log_table: Option<&[u8]>,
    ) -> E {
        let h = self.log.header();
        let (_, edge) = self.read_log_edge_from_table_or_store(&h, leaf, log_idx, log_table);
        edge
    }

    pub(crate) fn rewrite_overflow_log_entry_tombstone(
        &self,
        leaf: u32,
        entry_idx: u32,
    ) -> Result<(), LaraOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let log_h = self.log.header();
        let (prev, _) = self.read_log_edge_from_table_or_store(&log_h, leaf, entry_idx, None);
        let tombstone = E::tombstone_edge();
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            tombstone.write_to(&mut payload[..E::BYTES]);
            self.log
                .write_entry_with_header(&log_h, leaf, entry_idx, prev, &payload[..E::BYTES])
                .map_err(LaraOperationError::WriteLogFailed)?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            tombstone.write_to(&mut payload);
            self.log
                .write_entry_with_header(&log_h, leaf, entry_idx, prev, &payload)
                .map_err(LaraOperationError::WriteLogFailed)?;
        }
        Ok(())
    }

    pub(super) fn tombstone_overflow_log_delete_target<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vid: VertexId,
        v: V,
        target: DeleteTarget,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        E: CsrEdgeTombstone,
    {
        match target {
            DeleteTarget::Slab(offset) => {
                let rm_slot = v
                    .base_slot_start()
                    .checked_add(u64::from(offset))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.write_slot(rm_slot, E::tombstone_edge())
                    .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
            }
            DeleteTarget::Log(entry_idx) => {
                let leaf = leaf_segment(vertices.log_leaf_vertex(vid), edge_layout.segment_size);
                self.rewrite_overflow_log_entry_tombstone(leaf, entry_idx)?;
            }
        }
        vertices.set(vid, &v.after_slab_tombstone_delete());
        Ok(())
    }

    pub(super) fn insert_into_log_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vid: VertexId,
        log_owner: VertexId,
        v: V,
        next_degree: u32,
        edge: E,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let log_h = self.log.header();
        let idx = self.log.read_idx_with_header(&log_h, leaf);
        if idx < 0 || idx >= log_h.max_log_entries as i32 {
            return Err(LaraOperationError::SegmentLogFull);
        }
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::BYTES]);
            self.log
                .write_entry_with_header(
                    &log_h,
                    leaf,
                    idx as u32,
                    v.log_head(),
                    &payload[..E::BYTES],
                )
                .map_err(LaraOperationError::WriteLogFailed)?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            self.log
                .write_entry_with_header(&log_h, leaf, idx as u32, v.log_head(), &payload)
                .map_err(LaraOperationError::WriteLogFailed)?;
        }
        self.log.write_idx_with_header(&log_h, leaf, idx + 1);
        let _ = next_degree;
        let grown = v
            .with_log_head(idx)
            .try_grow_packed_slab_by_one()
            .map_err(|()| LaraOperationError::RowDegreeOverflow)?;
        vertices.set(vid, &grown);
        Ok(())
    }

    pub(crate) fn log_is_full_with_segment_size(&self, vid: VertexId, segment_size: u32) -> bool {
        let log_h = self.log.header();
        let leaf = leaf_segment(vid, segment_size);
        self.log.read_idx_with_header(&log_h, leaf) >= log_h.max_log_entries as i32
    }

    pub(crate) fn log_fill_ratio(&self, segment: SegmentId) -> f64 {
        let log_h = self.log.header();
        let idx = self
            .log
            .read_idx_with_header(&log_h, u32::from(segment))
            .max(0) as f64;
        let capacity = log_h.max_log_entries.max(1) as f64;
        idx / capacity
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::VertexCount;
    use crate::test_support::{TestEdge, vector_memory};
    use crate::traits::CsrEdge;

    fn fresh_edges() -> EdgeStore<TestEdge, crate::VectorMemory> {
        let edges = EdgeStore::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            1,
            0,
        )
        .unwrap();
        edges
            .grow_segment_tree_to(segment_tree_leaf_count(VertexCount::from(2u32), 1))
            .unwrap();
        edges
    }

    fn edge_payload(value: u32) -> Vec<u8> {
        let mut buf = vec![0u8; TestEdge::BYTES];
        TestEdge(value).write_to(&mut buf);
        buf
    }

    #[test]
    fn chain_walks_terminate_and_bound_on_corrupt_prev_cycle() {
        // A corrupt `prev` link forms a 3-entry cycle: head 2 -> 1 -> 0 -> 2 -> ...
        // Without the budget guard both walks would loop forever / grow unbounded.
        let edges = fresh_edges();
        let leaf = 0u32;
        let h = edges.log.header();
        let payload = edge_payload(7);
        edges
            .log
            .write_entry_with_header(&h, leaf, 0, 2, &payload)
            .unwrap();
        edges
            .log
            .write_entry_with_header(&h, leaf, 1, 0, &payload)
            .unwrap();
        edges
            .log
            .write_entry_with_header(&h, leaf, 2, 1, &payload)
            .unwrap();
        edges.log.write_idx_with_header(&h, leaf, 3);

        let chain = edges.overflow_log_chain_asc_indices(leaf, 2);
        assert_eq!(chain.len(), h.max_log_entries as usize);
        assert_eq!(
            edges.overflow_log_chain_len(leaf, 2),
            h.max_log_entries,
            "length walk is capped at the per-segment budget"
        );
    }

    #[test]
    fn chain_walks_return_full_valid_chain() {
        // Acyclic chain head 2 -> 1 -> 0 -> -1 must not be truncated by the guard.
        let edges = fresh_edges();
        let leaf = 0u32;
        let h = edges.log.header();
        let payload = edge_payload(7);
        edges
            .log
            .write_entry_with_header(&h, leaf, 0, -1, &payload)
            .unwrap();
        edges
            .log
            .write_entry_with_header(&h, leaf, 1, 0, &payload)
            .unwrap();
        edges
            .log
            .write_entry_with_header(&h, leaf, 2, 1, &payload)
            .unwrap();
        edges.log.write_idx_with_header(&h, leaf, 3);

        assert_eq!(edges.overflow_log_chain_asc_indices(leaf, 2), vec![0, 1, 2]);
        assert_eq!(edges.overflow_log_chain_len(leaf, 2), 3);
    }
}
