//! EdgeStore `log_mut` implementation.

use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    SegmentId, VertexId,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::log::HeaderV1 as LogHeaderV1;
use super::scan_iter::leaf_segment;
use super::{
    DeleteTarget, EdgeLayout, EdgeStore, INLINE_EDGE_BYTES, LOG_SRC_DEAD, LogEntryKind,
    decode_log_entry_kind, encode_delete_target,
};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub(crate) fn overflow_log_chain_len(&self, leaf: u32, head: i32) -> u32 {
        if head < 0 {
            return 0;
        }
        let h = self.log.header();
        let scratch_len = E::BYTES.max(1);
        let mut cur = head;
        let mut len = 0u32;
        while cur >= 0 {
            len = len.saturating_add(1);
            if E::BYTES <= INLINE_EDGE_BYTES {
                let mut scratch = [0u8; INLINE_EDGE_BYTES];
                let (prev, _) = self.log.read_entry_with_header(
                    &h,
                    leaf,
                    cur as u32,
                    &mut scratch[..scratch_len],
                );
                cur = prev;
            } else {
                let mut scratch = vec![0u8; E::BYTES];
                let (prev, _) = self
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
        let scratch_len = E::BYTES.max(1);
        while cur >= 0 {
            chain.push(cur as u32);
            if E::BYTES <= INLINE_EDGE_BYTES {
                let mut scratch = [0u8; INLINE_EDGE_BYTES];
                let (prev, _) = self.log.read_entry_with_header(
                    &h,
                    leaf,
                    cur as u32,
                    &mut scratch[..scratch_len],
                );
                cur = prev;
            } else {
                let mut scratch = vec![0u8; E::BYTES];
                let (prev, _) = self
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

    pub(crate) fn read_overflow_log_entry(&self, leaf: u32, entry_idx: u32) -> (i32, i32, E) {
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

        let mut deleted_log_indices: Vec<u32> = Vec::new();
        let mut deleted_slab_offsets: Vec<u32> = Vec::new();
        let mut log_entries: Vec<Option<E>> = Vec::new();
        let mut log_i = log_head;
        let mut budget = log_h.max_log_entries;
        while budget > 0 {
            budget -= 1;
            if log_i < 0 {
                return Ok((log_entries, deleted_slab_offsets));
            }
            let log_idx = log_i as u32;
            let (prev, src, edge) =
                self.read_log_edge_from_table_or_store(log_h, leaf, log_idx, log_table);
            log_i = prev;
            match decode_log_entry_kind(src) {
                LogEntryKind::Dead => {
                    log_entries.push(None);
                    continue;
                }
                LogEntryKind::Delete(target) => {
                    match target {
                        DeleteTarget::Slab(offset) => deleted_slab_offsets.push(offset),
                        DeleteTarget::Log(index) => deleted_log_indices.push(index),
                    }
                    continue;
                }
                LogEntryKind::Live => {}
            }
            if let Some(pos) = deleted_log_indices.iter().position(|&d| d == log_idx) {
                deleted_log_indices.swap_remove(pos);
                continue;
            }
            log_entries.push(Some(edge));
        }
        if log_i >= 0 {
            return Err(LaraOperationError::LogChainShort);
        }
        Ok((log_entries, deleted_slab_offsets))
    }

    pub(super) fn read_log_entry_src_tag(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_idx: u32,
        table: Option<&[u8]>,
    ) -> (i32, i32) {
        if let Some(buf) = table {
            let stride = log_h.stride as usize;
            if stride > 0 {
                let off = log_idx as usize * stride;
                if off + 8 <= buf.len() {
                    let prev = i32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                    let src = i32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
                    return (prev, src);
                }
            }
        }
        let mut buf = [0u8; 8];
        self.log
            .read_entry_with_header(log_h, leaf, log_idx, &mut buf)
    }

    pub(super) fn read_log_edge_from_table_or_store(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_idx: u32,
        table: Option<&[u8]>,
    ) -> (i32, i32, E) {
        if let Some(buf) = table {
            let stride = log_h.stride as usize;
            if stride > 0 {
                let off = log_idx as usize * stride;
                if off + stride <= buf.len() && off + 8 + E::BYTES <= buf.len() {
                    let prev = i32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                    let src = i32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
                    let edge = E::read_from(&buf[off + 8..off + 8 + E::BYTES]);
                    return (prev, src, edge);
                }
            }
        }
        let (prev, src) = self.read_log_entry_src_tag(log_h, leaf, log_idx, table);
        if E::BYTES <= 8 {
            let mut buf = [0u8; 8];
            let (_, _) =
                self.log
                    .read_entry_with_header(log_h, leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, src, E::read_from(&buf[..E::BYTES]))
        } else if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            let (_, _) =
                self.log
                    .read_entry_with_header(log_h, leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, src, E::read_from(&buf[..E::BYTES]))
        } else {
            let mut buf = vec![0u8; E::BYTES];
            let (_, _) = self
                .log
                .read_entry_with_header(log_h, leaf, log_idx, &mut buf);
            (prev, src, E::read_from(&buf))
        }
    }

    /// Overflow-log replay tags for descending scan (`None` = dead placeholder slot).
    pub(super) fn prefetch_descending_log_replay_tags(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<Option<()>>, Vec<u32>), LaraOperationError> {
        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let mut deleted_log_indices: Vec<u32> = Vec::new();
        let mut deleted_slab_offsets: Vec<u32> = Vec::new();
        let mut replay_tags: Vec<Option<()>> = Vec::new();
        let mut log_i = log_head;
        let mut budget = log_h.max_log_entries;
        while budget > 0 {
            budget -= 1;
            if log_i < 0 {
                return Ok((replay_tags, deleted_slab_offsets));
            }
            let log_idx = log_i as u32;
            let (prev, src) = self.read_log_entry_src_tag(log_h, leaf, log_idx, log_table);
            log_i = prev;
            match decode_log_entry_kind(src) {
                LogEntryKind::Dead => {
                    replay_tags.push(None);
                    continue;
                }
                LogEntryKind::Delete(target) => match target {
                    DeleteTarget::Slab(offset) => deleted_slab_offsets.push(offset),
                    DeleteTarget::Log(index) => deleted_log_indices.push(index),
                },
                LogEntryKind::Live => {}
            }
            if let Some(pos) = deleted_log_indices.iter().position(|&d| d == log_idx) {
                deleted_log_indices.swap_remove(pos);
                continue;
            }
            replay_tags.push(Some(()));
        }
        if log_i >= 0 {
            return Err(LaraOperationError::LogChainShort);
        }
        Ok((replay_tags, deleted_slab_offsets))
    }

    /// Overflow-log inserted-edge replay tags for ascending scan (`None` consumes a slot).
    pub(super) fn prefetch_ascending_log_inserted_tags(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<Option<()>>, Vec<u32>), LaraOperationError> {
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
            let (prev, src) = self.read_log_entry_src_tag(log_h, leaf, log_i as u32, log_table);
            entries.push((log_i as u32, src));
            log_i = prev;
            steps = steps
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        entries.reverse();

        let mut inserted: Vec<(DeleteTarget, Option<()>)> = Vec::new();
        let mut deleted_slab_offsets = Vec::new();
        for (log_idx, src) in entries {
            match decode_log_entry_kind(src) {
                LogEntryKind::Dead => inserted.push((DeleteTarget::Log(log_idx), None)),
                LogEntryKind::Delete(target) => match target {
                    DeleteTarget::Slab(offset) => deleted_slab_offsets.push(offset),
                    DeleteTarget::Log(_) => {
                        if let Some(index) = inserted
                            .iter()
                            .position(|(candidate, _)| *candidate == target)
                        {
                            inserted.remove(index);
                        }
                    }
                },
                LogEntryKind::Live => inserted.push((DeleteTarget::Log(log_idx), Some(()))),
            }
        }

        Ok((
            inserted.into_iter().map(|(_, tag)| tag).collect(),
            deleted_slab_offsets,
        ))
    }

    pub(super) fn insert_delete_into_log_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vid: VertexId,
        v: V,
        target: DeleteTarget,
        edge: E,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let leaf = leaf_segment(vertices.log_leaf_vertex(vid), edge_layout.segment_size);
        let log_h = self.log.header();
        let idx = self.log.read_idx_with_header(&log_h, leaf);
        if idx < 0 || idx >= log_h.max_log_entries as i32 {
            return Err(LaraOperationError::SegmentLogFull);
        }
        let src = encode_delete_target(target)?;
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::BYTES]);
            self.log
                .write_entry_with_header(
                    &log_h,
                    leaf,
                    idx as u32,
                    v.log_head(),
                    src,
                    &payload[..E::BYTES],
                )
                .map_err(LaraOperationError::WriteLogFailed)?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            self.log
                .write_entry_with_header(&log_h, leaf, idx as u32, v.log_head(), src, &payload)
                .map_err(LaraOperationError::WriteLogFailed)?;
        }
        self.log.write_idx_with_header(&log_h, leaf, idx + 1);
        vertices.set(vid, &v.with_log_head(idx).after_slab_tombstone_delete());
        Ok(())
    }

    pub(crate) fn mark_overflow_log_entry_dead(
        &self,
        leaf: u32,
        entry_idx: u32,
    ) -> Result<(), LaraOperationError> {
        let log_h = self.log.header();
        let (prev, _, _) = self.read_log_edge_from_table_or_store(&log_h, leaf, entry_idx, None);
        let payload = vec![0u8; E::BYTES];
        self.log
            .write_entry_with_header(&log_h, leaf, entry_idx, prev, LOG_SRC_DEAD, &payload)
            .map_err(LaraOperationError::WriteLogFailed)
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
        let src = i32::try_from(u32::from(log_owner))
            .map_err(|_| LaraOperationError::VertexIdExceedsI32)?;
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::BYTES]);
            self.log
                .write_entry_with_header(
                    &log_h,
                    leaf,
                    idx as u32,
                    v.log_head(),
                    src,
                    &payload[..E::BYTES],
                )
                .map_err(LaraOperationError::WriteLogFailed)?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            self.log
                .write_entry_with_header(&log_h, leaf, idx as u32, v.log_head(), src, &payload)
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
