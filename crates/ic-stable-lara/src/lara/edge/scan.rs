//! EdgeStore `scan` implementation.

use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    VertexId,
    traits::{CsrEdge, CsrVertex, CsrVertexTombstoneScan},
};
use ic_stable_structures::Memory;

use super::scan_iter::{
    AscOutEdgesIter, LogBackedDescIter, OutEdgeSlabChunk, OutEdgeSlabIter, leaf_segment,
};
use super::{
    DeleteTarget, EdgeStore, LogEntryKind, OUT_EDGE_SLAB_PREFETCH_MIN_BYTES, OutEdgeVisitWindow,
    OutEdgesIter, decode_log_entry_kind,
};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub(crate) fn asc_out_edges<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<Vec<E>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        Ok(self
            .collect_out_edge_refs_slot_order(vertices, vid)?
            .into_iter()
            .map(|(_, edge)| edge)
            .collect())
    }

    pub(crate) fn visit_out_edges<V, A, Match, Visit>(
        &self,
        vertices: &A,
        vid: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        mut matches: Match,
        mut visit: Visit,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        let mut window = OutEdgeVisitWindow::new(offset, limit);
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(());
        }
        if v.log_head() < 0 {
            let mut it =
                OutEdgeSlabIter::try_new(self, v.base_slot_start(), v.stored_degree(), v.degree())?;
            let has_raw = raw_matches.is_some();
            while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                if has_raw {
                    if matches(&edge) && !window.emit_edge(edge, &mut visit) {
                        return Ok(());
                    }
                } else if matches(&edge) && !window.emit_edge(edge, &mut visit) {
                    return Ok(());
                }
            }
            return Ok(());
        }

        let walk = self.log_backed_desc_edges_iter(vertices, vid)?;
        for edge in walk {
            if matches(&edge) && !window.emit_edge(edge, &mut visit) {
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) fn find_first_out_edge_matching<V, A, Match>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: &mut Match,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
        Match: FnMut(&E) -> bool,
    {
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(None);
        }
        if v.log_head() < 0 {
            let mut it =
                OutEdgeSlabIter::try_new(self, v.base_slot_start(), v.stored_degree(), v.degree())?;
            while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                if matches(&edge) {
                    return Ok(Some(edge));
                }
            }
            return Ok(None);
        }

        let walk = self.log_backed_desc_edges_iter(vertices, vid)?;
        for edge in walk {
            if matches(&edge) {
                return Ok(Some(edge));
            }
        }
        Ok(None)
    }

    pub(crate) fn has_out_edges<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<bool, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        Ok(v.degree() > 0)
    }

    pub(crate) fn log_backed_desc_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<LogBackedDescIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let stored = v.stored_degree();
        let slab_count = on_slab.min(stored);
        let nbytes_slab = (slab_count as usize)
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let slab_chunk = if nbytes_slab >= OUT_EDGE_SLAB_PREFETCH_MIN_BYTES {
            Some(OutEdgeSlabChunk {
                buf: Vec::new(),
                chunk_low: 0,
                chunk_high: 0,
            })
        } else {
            None
        };
        let log_header = self.log.header();
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let reserved_log_slots = u32::try_from(
            self.overflow_log_chain_asc_indices(leaf, v.log_head())
                .len(),
        )
        .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        Ok(LogBackedDescIter {
            store: self,
            leaf,
            next_log: v.log_head(),
            remaining_log: log_header.max_log_entries,
            base_slot_start: v.base_slot_start(),
            remaining_slab: slab_count,
            yield_remaining: v.degree(),
            log_header,
            log_table: None,
            slab_chunk,
            deleted_log_indices: Vec::new(),
            deleted_slab_offsets: Vec::new(),
            sorted_slab_deletes: false,
            next_log_slot: slab_count
                .saturating_add(reserved_log_slots)
                .saturating_sub(1),
        })
    }

    pub(crate) fn out_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        // See `asc_out_edges`: allow enumeration for tombstones that
        // still have pending edge material (rebalance during vertex delete).
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(OutEdgesIter {
                store: self,
                base_slot_start: v.base_slot_start(),
                remaining_slab: 0,
                yield_remaining: 0,
                log_entries: Vec::new(),
                log_pos: 0,
                next_log_slot: 0,
                slab_chunk: None,
                deleted_slab_offsets: Vec::new(),
            });
        }
        // Clean rows: the full neighborhood is on the slab, so the iterator never
        // walks the overflow log. Skip `edge_layout()` (full slab header read) and
        // log metadata.
        if v.log_head() < 0 {
            let stored = v.stored_degree();
            let live = v.degree();
            let nbytes = (stored as usize)
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let slab_chunk = if nbytes >= OUT_EDGE_SLAB_PREFETCH_MIN_BYTES {
                Some(OutEdgeSlabChunk {
                    buf: Vec::new(),
                    chunk_low: 0,
                    chunk_high: 0,
                })
            } else {
                None
            };
            return Ok(OutEdgesIter {
                store: self,
                base_slot_start: v.base_slot_start(),
                remaining_slab: stored,
                yield_remaining: live,
                log_entries: Vec::new(),
                log_pos: 0,
                next_log_slot: 0,
                slab_chunk,
                deleted_slab_offsets: Vec::new(),
            });
        }

        let edge_layout = self.edge_layout();
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let stored = v.stored_degree();
        let slab_count = on_slab.min(stored);
        let nbytes_slab = (slab_count as usize)
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let slab_chunk = if nbytes_slab >= OUT_EDGE_SLAB_PREFETCH_MIN_BYTES {
            Some(OutEdgeSlabChunk {
                buf: Vec::new(),
                chunk_low: 0,
                chunk_high: 0,
            })
        } else {
            None
        };

        let log_header = self.log.header();
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let (log_entries, mut deleted_slab_offsets) =
            self.prefetch_descending_log_entries(&log_header, leaf, v.log_head())?;
        deleted_slab_offsets.sort_unstable();
        let reserved_log_slots =
            u32::try_from(log_entries.len()).map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        Ok(OutEdgesIter {
            store: self,
            base_slot_start: v.base_slot_start(),
            remaining_slab: slab_count,
            yield_remaining: v.degree(),
            log_entries,
            log_pos: 0,
            next_log_slot: slab_count
                .saturating_add(reserved_log_slots)
                .saturating_sub(1),
            slab_chunk,
            deleted_slab_offsets,
        })
    }

    pub(crate) fn desc_out_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        self.out_edges_iter(vertices, vid)
    }

    pub(crate) fn asc_out_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<AscOutEdgesIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(AscOutEdgesIter::empty(self));
        }
        if v.log_head() < 0 {
            return Ok(AscOutEdgesIter::slab_only(
                self,
                v.base_slot_start(),
                v.stored_degree(),
                v.degree(),
            ));
        }

        let edge_layout = self.edge_layout();
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let slab_count = on_slab.min(v.stored_degree());
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

        let mut inserted = Vec::new();
        let mut deleted_slab_offsets = Vec::new();
        for (log_idx, src, edge) in entries {
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
                LogEntryKind::Live if edge.is_deleted_slot() => {
                    inserted.push((DeleteTarget::Log(log_idx), None));
                }
                LogEntryKind::Live => inserted.push((DeleteTarget::Log(log_idx), Some(edge))),
            }
        }

        Ok(AscOutEdgesIter::with_reserved_log_replay(
            self,
            v.base_slot_start(),
            slab_count,
            v.degree(),
            deleted_slab_offsets,
            inserted.into_iter().map(|(_, edge)| edge).collect(),
        ))
    }

    pub(crate) fn prefetch_overflow_log_replay_desc(
        &self,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<Option<u32>>, Vec<u32>, Vec<u8>), LaraOperationError> {
        let log_h = self.log.header();
        self.prefetch_descending_log_replay_tags(&log_h, leaf, log_head)
    }

    pub(crate) fn prefetch_overflow_log_inserted_tags_asc(
        &self,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<Option<u32>>, Vec<u32>, Vec<u8>), LaraOperationError> {
        let log_h = self.log.header();
        self.prefetch_ascending_log_inserted_tags(&log_h, leaf, log_head)
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::lara::vertex::{Vertex, VertexStore};
    use crate::test_support::{TestEdge, vector_memory};
    use crate::{VectorMemory, VertexCount, VertexId};
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn edge_store_reads_slab_then_log_neighborhood() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();
        vertices
            .push(Vertex::from_parts(1, 0, 0, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
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
        assert_eq!(edges.span_meta_store().len(), 2);

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(
            edges.asc_out_edges(&vertices, VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(
            edges
                .out_edges_iter(&vertices, VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(11), TestEdge(10)]
        );
        assert_eq!(
            edges
                .desc_out_edges_iter(&vertices, VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(11), TestEdge(10)]
        );
        assert_eq!(
            edges
                .asc_out_edges_iter(&vertices, VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(vertices.get(VertexId::from(0)).live_edges, 2);
        assert!(vertices.get(VertexId::from(0)).log_head() >= 0);
    }

    #[test]
    fn edge_store_uses_csr_neighbor_bases_for_slab_space() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();
        vertices
            .push(Vertex::from_parts(2, 0, 0, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            vector_memory(),
            4,
            1,
            0,
        )
        .unwrap();
        edges
            .grow_segment_tree_to(segment_tree_leaf_count(VertexCount::from(2u32), 1))
            .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(vertices.get(VertexId::from(0)).live_edges, 2);
        assert_eq!(vertices.get(VertexId::from(0)).log_head(), -1);
        assert_eq!(
            edges.asc_out_edges(&vertices, VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }

    #[test]
    fn out_edges_iter_nth_pure_slab_matches_scan_order() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();
        vertices
            .push(Vertex::from_parts(2, 0, 0, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            vector_memory(),
            4,
            1,
            0,
        )
        .unwrap();
        edges
            .grow_segment_tree_to(segment_tree_leaf_count(VertexCount::from(2u32), 1))
            .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        let scan = edges
            .out_edges_iter(&vertices, VertexId::from(0))
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(scan, vec![TestEdge(11), TestEdge(10)]);

        let mut it = edges.out_edges_iter(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.next(), Some(TestEdge(11)));
        let mut it = edges.out_edges_iter(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.nth(1), Some(TestEdge(10)));
        let mut it = edges.out_edges_iter(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.nth(2), None);
    }

    #[test]
    fn edge_store_scan_uses_base_and_degree_only() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 2, 2, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            vector_memory(),
            2,
            1,
            0,
        )
        .unwrap();
        edges.write_slot(0, TestEdge(10)).unwrap();
        edges.write_slot(1, TestEdge(11)).unwrap();

        assert_eq!(
            edges.asc_out_edges(&vertices, VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }
}
