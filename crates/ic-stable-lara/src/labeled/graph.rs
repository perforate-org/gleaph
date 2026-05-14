//! Single-orientation labeled LARA graph without edge overflow logs.

use crate::{
    VertexCount, VertexId,
    labeled::{
        edge_slab::{EdgeSlabStore, InitError as EdgeInitError},
        record::{LabelBucket, LabelId, LabeledVertex},
        row_store::{InitError as RowInitError, RowStore},
        traits::LabeledCsrVertex,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;
use std::{fmt, marker::PhantomData};

const VERTEX_MAGIC: [u8; 3] = *b"LLV";
const BUCKET_MAGIC: [u8; 3] = *b"LLB";

/// Errors returned by labeled graph operations.
#[derive(Debug)]
pub enum LabeledOperationError {
    VertexOutOfRange { vid: VertexId, len: VertexCount },
    EdgeSlabFull,
    BucketSlabFull,
    InvalidDefaultBypass,
}

impl fmt::Display for LabeledOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
            Self::EdgeSlabFull => write!(f, "labeled edge slab is full"),
            Self::BucketSlabFull => write!(f, "labeled bucket slab is full"),
            Self::InvalidDefaultBypass => write!(
                f,
                "default-label bypass requires exactly one default adjacency label"
            ),
        }
    }
}

impl std::error::Error for LabeledOperationError {}

/// Errors returned when reopening a labeled graph.
#[derive(Debug)]
pub enum InitError {
    Vertices(RowInitError),
    Buckets(RowInitError),
    Edges(EdgeInitError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vertices(e) => write!(f, "vertex init failed: {e}"),
            Self::Buckets(e) => write!(f, "bucket init failed: {e}"),
            Self::Edges(e) => write!(f, "edge init failed: {e}"),
        }
    }
}

impl std::error::Error for InitError {}

/// Single-orientation multi-level labeled CSR graph.
pub struct LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    vertices: RowStore<LabeledVertex, M>,
    buckets: RowStore<LabelBucket, M>,
    edges: EdgeSlabStore<E, M>,
    default_label: LabelId,
    _marker: PhantomData<E>,
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Creates a fresh labeled graph over the supplied stable memories.
    pub fn new(
        vertices: M,
        buckets: M,
        edges: M,
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, crate::GrowFailed> {
        Ok(Self {
            vertices: RowStore::new(vertices, VERTEX_MAGIC)?,
            buckets: RowStore::new(buckets, BUCKET_MAGIC)?,
            edges: EdgeSlabStore::new(edges, elem_capacity)?,
            default_label,
            _marker: PhantomData,
        })
    }

    /// Opens a labeled graph from stable memories, creating it when the edge slab is empty.
    pub fn init(
        vertices: M,
        buckets: M,
        edges: M,
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, InitError> {
        let edge_store = if edges.size() == 0 {
            EdgeSlabStore::new(edges, elem_capacity)
                .map_err(|_| InitError::Edges(EdgeInitError::OutOfMemory))?
        } else {
            EdgeSlabStore::init(edges).map_err(InitError::Edges)?
        };
        Ok(Self {
            vertices: RowStore::init(vertices, VERTEX_MAGIC).map_err(InitError::Vertices)?,
            buckets: RowStore::init(buckets, BUCKET_MAGIC).map_err(InitError::Buckets)?,
            edges: edge_store,
            default_label,
            _marker: PhantomData,
        })
    }

    pub fn vertices(&self) -> &RowStore<LabeledVertex, M> {
        &self.vertices
    }

    pub fn buckets(&self) -> &RowStore<LabelBucket, M> {
        &self.buckets
    }

    pub fn edges(&self) -> &EdgeSlabStore<E, M> {
        &self.edges
    }

    pub fn default_label(&self) -> LabelId {
        self.default_label
    }

    pub fn vertex_count(&self) -> VertexCount {
        VertexCount::from(self.vertices.len())
    }

    fn ensure_vertex(&self, vid: VertexId) -> Result<(), LabeledOperationError> {
        if u32::from(vid) >= self.vertices.len() {
            return Err(LabeledOperationError::VertexOutOfRange {
                vid,
                len: self.vertex_count(),
            });
        }
        Ok(())
    }

    /// Appends a new vertex row.
    pub fn push_vertex(&self, vertex: LabeledVertex) -> Result<VertexId, crate::GrowFailed> {
        self.vertices.push(vertex)
    }

    /// Inserts one edge under `label_id` at `src`.
    pub fn insert_edge(
        &self,
        src: VertexId,
        label_id: LabelId,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(src)?;
        let mut vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.default_label {
                return Err(LabeledOperationError::InvalidDefaultBypass);
            }
            let slot = self
                .edges
                .allocate_slot()
                .map_err(|_| LabeledOperationError::EdgeSlabFull)?;
            self.edges
                .write_slot(slot, edge)
                .map_err(|_| LabeledOperationError::EdgeSlabFull)?;
            if vertex.row_count == 0 {
                vertex.base_slot_start = slot;
            }
            vertex.row_count = vertex.row_count.saturating_add(1);
            self.vertices.set(src, &vertex);
            return Ok(());
        }

        let bucket_idx = self.find_or_create_bucket(src, &mut vertex, label_id)?;
        let bucket = self.buckets.get(VertexId::from(bucket_idx as u32));
        let slot = if bucket.edge_len == 0 {
            self.edges
                .allocate_slot()
                .map_err(|_| LabeledOperationError::EdgeSlabFull)?
        } else {
            bucket.edge_start.saturating_add(u64::from(bucket.edge_len))
        };
        if slot >= self.edges.header().elem_capacity {
            return Err(LabeledOperationError::EdgeSlabFull);
        }
        self.edges
            .write_slot(slot, edge)
            .map_err(|_| LabeledOperationError::EdgeSlabFull)?;
        let updated_bucket = LabelBucket {
            edge_start: if bucket.edge_len == 0 {
                slot
            } else {
                bucket.edge_start
            },
            edge_len: bucket.edge_len.saturating_add(1),
            ..bucket
        };
        self.buckets
            .set(VertexId::from(bucket_idx as u32), &updated_bucket);
        self.vertices.set(src, &vertex);
        Ok(())
    }

    fn find_or_create_bucket(
        &self,
        src: VertexId,
        vertex: &mut LabeledVertex,
        label_id: LabelId,
    ) -> Result<u64, LabeledOperationError> {
        let start = vertex.base_slot_start;
        let end = start.saturating_add(u64::from(vertex.row_count));
        for idx in start..end {
            let bucket = self.buckets.get(VertexId::from(idx as u32));
            if bucket.label_id == label_id {
                return Ok(idx);
            }
            if bucket.label_id > label_id {
                break;
            }
        }
        let new_bucket = LabelBucket {
            label_id,
            reserved: 0,
            edge_start: 0,
            edge_len: 0,
            _pad: 0,
        };
        let new_idx = self
            .buckets
            .push(new_bucket)
            .map_err(|_| LabeledOperationError::BucketSlabFull)?;
        if vertex.row_count == 0 {
            vertex.base_slot_start = u64::from(u32::from(new_idx));
        }
        vertex.row_count = vertex.row_count.saturating_add(1);
        self.vertices.set(src, vertex);
        Ok(u64::from(u32::from(new_idx)))
    }

    /// Enables default-label bypass for `src` when it has exactly one default label.
    pub fn enable_default_edge_bypass(&self, src: VertexId) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        if vertex.row_count > 1 {
            return Err(LabeledOperationError::InvalidDefaultBypass);
        }
        if vertex.row_count == 1 {
            let bucket = self
                .buckets
                .get(VertexId::from(vertex.base_slot_start as u32));
            if bucket.label_id != self.default_label {
                return Err(LabeledOperationError::InvalidDefaultBypass);
            }
            let updated = vertex
                .with_default_edge_labeled(true)
                .with_base_slot_start(bucket.edge_start)
                .with_degree(bucket.edge_len);
            self.vertices.set(src, &updated);
        } else {
            self.vertices
                .set(src, &vertex.with_default_edge_labeled(true));
        }
        Ok(())
    }

    /// Iterates all outgoing edges for one label without per-edge label checks.
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: LabelId,
    ) -> Result<impl Iterator<Item = E> + '_, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.default_label {
                return Ok(Vec::new().into_iter());
            }
            let start = vertex.base_slot_start;
            let end = start.saturating_add(u64::from(vertex.row_count));
            let edges = (start..end)
                .map(|slot| self.edges.read_slot(slot))
                .collect::<Vec<_>>();
            return Ok(edges.into_iter());
        }
        let start = vertex.base_slot_start;
        let end = start.saturating_add(u64::from(vertex.row_count));
        for idx in start..end {
            let bucket = self.buckets.get(VertexId::from(idx as u32));
            if bucket.label_id == label_id {
                let edge_start = bucket.edge_start;
                let edge_end = edge_start.saturating_add(u64::from(bucket.edge_len));
                let edges = (edge_start..edge_end)
                    .map(|slot| self.edges.read_slot(slot))
                    .collect::<Vec<_>>();
                return Ok(edges.into_iter());
            }
        }
        Ok(Vec::new().into_iter())
    }

    /// Iterates all outgoing edges across every label bucket.
    pub fn iter_out_edges(
        &self,
        src: VertexId,
    ) -> Result<impl Iterator<Item = E> + '_, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            let start = vertex.base_slot_start;
            let end = start.saturating_add(u64::from(vertex.row_count));
            let edges = (start..end)
                .map(|slot| self.edges.read_slot(slot))
                .collect::<Vec<_>>();
            return Ok(edges.into_iter());
        }
        let start = vertex.base_slot_start;
        let end = start.saturating_add(u64::from(vertex.row_count));
        let mut out = Vec::new();
        for idx in start..end {
            let bucket = self.buckets.get(VertexId::from(idx as u32));
            let edge_start = bucket.edge_start;
            let edge_end = edge_start.saturating_add(u64::from(bucket.edge_len));
            out.extend((edge_start..edge_end).map(|slot| self.edges.read_slot(slot)));
        }
        Ok(out.into_iter())
    }

    /// Removes the first edge that satisfies `matches`.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: LabelId,
        mut matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.default_label {
                return Ok(None);
            }
            return self.remove_from_edge_range(
                src,
                vertex.base_slot_start,
                vertex.row_count,
                &mut matches,
            );
        }
        let start = vertex.base_slot_start;
        let end = start.saturating_add(u64::from(vertex.row_count));
        for idx in start..end {
            let bucket_idx = idx as u32;
            let bucket = self.buckets.get(VertexId::from(bucket_idx));
            if bucket.label_id != label_id {
                continue;
            }
            if let Some(removed) =
                self.remove_from_edge_range(src, bucket.edge_start, bucket.edge_len, &mut matches)?
            {
                let new_len = bucket.edge_len.saturating_sub(1);
                self.buckets
                    .set(VertexId::from(bucket_idx), &bucket.with_degree(new_len));
                return Ok(Some(removed));
            }
            return Ok(None);
        }
        Ok(None)
    }

    fn remove_from_edge_range<F>(
        &self,
        src: VertexId,
        start: u64,
        len: u32,
        matches: &mut F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        if len == 0 {
            return Ok(None);
        }
        let end = start.saturating_add(u64::from(len));
        for slot in start..end {
            let edge = self.edges.read_slot(slot);
            if matches(&edge) {
                let last_slot = end - 1;
                if slot != last_slot {
                    let last = self.edges.read_slot(last_slot);
                    self.edges
                        .write_slot(slot, last)
                        .map_err(|_| LabeledOperationError::EdgeSlabFull)?;
                }
                let vertex = self.vertices.get(src).with_degree(len.saturating_sub(1));
                self.vertices.set(src, &vertex);
                return Ok(Some(edge));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VertexId, test_support::vector_memory, traits::CsrEdge};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestEdge {
        target: u32,
    }

    impl CsrEdge for TestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self {
                target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            }
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.target)
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self {
                target: u32::from(vid),
            }
        }
    }

    fn test_graph() -> LabeledLaraGraph<TestEdge, Rc<RefCell<Vec<u8>>>> {
        let default_label = LabelId::from_raw(1);
        let graph = LabeledLaraGraph::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            128,
            default_label,
        )
        .unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
    }

    #[test]
    fn labeled_insert_and_iter_by_label() {
        let graph = test_graph();
        let road = LabelId::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        let walk = LabelId::from_raw(3);
        graph
            .insert_edge(VertexId::from(0), walk, TestEdge { target: 20 })
            .unwrap();

        let road_edges = graph
            .iter_edges_for_label(VertexId::from(0), road)
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(
            road_edges,
            vec![TestEdge { target: 10 }, TestEdge { target: 11 }]
        );
        assert_eq!(
            graph
                .iter_out_edges(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![
                TestEdge { target: 10 },
                TestEdge { target: 11 },
                TestEdge { target: 20 },
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn default_bypass_points_directly_into_edge_csr() {
        let graph = test_graph();
        graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                graph.default_label(),
                TestEdge { target: 7 },
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.row_count, 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge { target: 7 }]
        );
    }

    #[test]
    fn remove_edge_uses_unordered_swap_remove() {
        let graph = test_graph();
        let road = LabelId::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 12 })
            .unwrap();
        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 11)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge { target: 10 }, TestEdge { target: 12 }]
        );
    }
}
