//! Logical delete, degree updates, GC queue, and filtered iterators.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_csr::{
    Bound, CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrGraphError, CsrGraphWithGcQueue,
    CsrVertex, CsrVertexColumn, CsrVertexTombstone, Storable, VectorMemory,
};

const DEG_TOMB: u32 = 1u32 << 31;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TV {
    slot_base: u64,
    deg: u32,
    log_head: i32,
}

impl CsrVertex for TV {
    fn base_slot_start(&self) -> u64 {
        self.slot_base
    }
    fn degree(&self) -> u32 {
        self.deg & !DEG_TOMB
    }
    fn with_base_slot_start(self, start: u64) -> Self {
        Self {
            slot_base: start,
            ..self
        }
    }
    fn with_degree(self, degree: u32) -> Self {
        Self {
            deg: (self.deg & DEG_TOMB) | (degree & !DEG_TOMB),
            ..self
        }
    }
    fn log_head(self) -> i32 {
        self.log_head
    }
    fn with_log_head(self, idx: i32) -> Self {
        Self {
            log_head: idx,
            ..self
        }
    }
}

impl CsrVertexTombstone for TV {
    fn is_tombstone(&self) -> bool {
        (self.deg & DEG_TOMB) != 0
    }

    fn with_tombstone(self, tombstone: bool) -> Self {
        Self {
            deg: if tombstone {
                self.deg | DEG_TOMB
            } else {
                self.deg & !DEG_TOMB
            },
            ..self
        }
    }
}

impl Storable for TV {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.slot_base.to_le_bytes());
        b[8..12].copy_from_slice(&self.deg.to_le_bytes());
        b[12..16].copy_from_slice(&self.log_head.to_le_bytes());
        Cow::Owned(b.to_vec())
    }
    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }
    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let s = bytes.as_ref();
        Self {
            slot_base: u64::from_le_bytes(s[0..8].try_into().unwrap()),
            deg: u32::from_le_bytes(s[8..12].try_into().unwrap()),
            log_head: i32::from_le_bytes(s[12..16].try_into().unwrap()),
        }
    }
    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TE([u8; 4]);

impl CsrEdge for TE {
    const EDGE_BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self(bytes.try_into().unwrap())
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes.copy_from_slice(&self.0);
    }

    fn neighbor_vid(&self) -> usize {
        self.0[0] as usize
    }

    fn with_neighbor_vid(self, vid: usize) -> Self {
        let mut b = self.0;
        b[0] = vid as u8;
        Self(b)
    }
}

impl CsrEdgeTombstone for TE {
    fn is_tombstone(&self) -> bool {
        self.0[2] != 0
    }

    fn with_tombstone(self, tombstone: bool) -> Self {
        let mut b = self.0;
        b[2] = if tombstone { 1 } else { 0 };
        Self(b)
    }
}

impl CsrEdgeUndirected for TE {
    fn is_undirected(&self) -> bool {
        self.0[1] != 0
    }

    fn with_undirected(self, undirected: bool) -> Self {
        let mut b = self.0;
        b[1] = if undirected { 1 } else { 0 };
        Self(b)
    }
}

fn vm() -> VectorMemory {
    Rc::new(RefCell::new(Vec::new()))
}

fn empty_vertex() -> TV {
    TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }
}

#[test]
fn delete_edge_tombstone_gc_and_degrees() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        64,
        1,
        8,
        0,
    )
    .expect("format");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();

    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    g.insert_directed(1, 2, TE([2, 0, 0, 0])).unwrap();

    assert_eq!(
        g.graph()
            .forward_dgap()
            .vertices
            .col_get(0)
            .unwrap()
            .degree(),
        1
    );
    assert_eq!(
        g.graph()
            .reverse_dgap()
            .vertices
            .col_get(1)
            .unwrap()
            .degree(),
        1
    );

    g.delete_edge_directed(0, 1).unwrap();
    assert_eq!(
        g.graph()
            .forward_dgap()
            .vertices
            .col_get(0)
            .unwrap()
            .degree(),
        0
    );
    assert_eq!(
        g.graph()
            .reverse_dgap()
            .vertices
            .col_get(1)
            .unwrap()
            .degree(),
        0
    );

    assert!(g.work_queue_len() >= 1);
    let n = g.gc_step(8).expect("gc");
    assert!(n >= 1);

    let out0: Vec<_> = g
        .out_edges_logical(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(out0.is_empty());
}

#[test]
fn delete_vertex_hides_edges_until_gc() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        64,
        1,
        8,
        0,
    )
    .expect("format");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    g.insert_directed(1, 0, TE([0, 0, 0, 0])).unwrap();

    assert_eq!(
        g.graph()
            .forward_dgap()
            .vertices
            .col_get(1)
            .unwrap()
            .degree(),
        1
    );
    g.delete_vertex(0).unwrap();
    assert!(
        g.graph()
            .forward_dgap()
            .vertices
            .col_get(0)
            .unwrap()
            .is_tombstone()
    );

    let out1: Vec<_> = g
        .out_edges_logical(1)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(out1.is_empty());

    let _ = g.gc_step(16).expect("gc");
}

#[test]
fn insert_rejects_tombstone_endpoint() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        64,
        1,
        8,
        0,
    )
    .expect("format");

    for _ in 0..2 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.delete_vertex(0).unwrap();

    let e = g.insert_directed(0, 1, TE([1, 0, 0, 0]));
    assert!(matches!(
        e,
        Err(CsrGraphError::EndpointTombstone { vid: 0 })
    ));
}

#[test]
fn insert_rejects_duplicate_neighbor_slot() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        vm(),
        64,
        1,
        8,
        0,
    )
    .expect("format");

    for _ in 0..2 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();

    let e = g.insert_directed(0, 1, TE([1, 0, 0, 0]));
    assert!(matches!(
        e,
        Err(CsrGraphError::AdjacencySlotOccupied { src: 0, dst: 1 })
    ));
}
