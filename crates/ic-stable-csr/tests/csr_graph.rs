//! [`CsrGraph::format_new`] and bidirectional insert / neighborhood iterators.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_csr::{
    Bound, CsrEdge, CsrEdgeUndirected, CsrGraph, CsrGraphError, CsrVertex, Storable, VectorMemory,
};

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
        self.deg
    }
    fn with_base_slot_start(self, start: u64) -> Self {
        Self {
            slot_base: start,
            ..self
        }
    }
    fn with_degree(self, degree: u32) -> Self {
        Self {
            deg: degree,
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

/// `[0]` = neighbor vid (u8), `[1]` = undirected flag (0/1), rest padding.
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
fn format_new_directed_transpose_neighbors() {
    let g = CsrGraph::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 64, 1, 8, 0)
        .expect("format_new");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();

    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    g.insert_directed(1, 2, TE([2, 0, 0, 0])).unwrap();

    let out0: Vec<_> = g.out_edges(0).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(out0, vec![TE([1, 0, 0, 0])]);

    let out1: Vec<_> = g.out_edges(1).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(out1, vec![TE([2, 0, 0, 0])]);

    let in1: Vec<_> = g.in_edges(1).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(in1, vec![TE([0, 0, 0, 0])]);

    let in2: Vec<_> = g.in_edges(2).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(in2, vec![TE([1, 0, 0, 0])]);
}

#[test]
fn insert_directed_rejects_undirected_flag_via_specialization() {
    let g =
        CsrGraph::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 32, 1, 8, 0).unwrap();

    g.insert_vertex(empty_vertex()).unwrap();
    g.insert_vertex(empty_vertex()).unwrap();
    g.sync_pma_meta().unwrap();

    let e = TE([1, 1, 0, 0]).with_undirected(true);
    let err = g.insert_directed(0, 1, e).unwrap_err();
    assert_eq!(err, CsrGraphError::UndirectedEdgeInDirectedInsert);
}

#[test]
fn insert_undirected_sets_flag_and_symmetric_degrees() {
    let g =
        CsrGraph::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 128, 1, 8, 0).unwrap();

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();

    g.insert_undirected(0, 2, TE([0, 0, 0, 0]).with_neighbor_vid(2))
        .unwrap();

    let out0: Vec<_> = g.out_edges(0).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(out0.len(), 1);
    assert_eq!(out0[0].neighbor_vid(), 2);
    assert!(out0[0].is_undirected());

    let out2: Vec<_> = g.out_edges(2).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(out2.len(), 1);
    assert_eq!(out2[0].neighbor_vid(), 0);
    assert!(out2[0].is_undirected());

    let in2: Vec<_> = g.in_edges(2).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(in2.len(), 1);
    assert_eq!(in2[0].neighbor_vid(), 0);

    let in0: Vec<_> = g.in_edges(0).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(in0.len(), 1);
    assert_eq!(in0[0].neighbor_vid(), 2);
}

#[test]
fn neighbor_mismatch_on_directed_insert() {
    let g =
        CsrGraph::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 32, 1, 8, 0).unwrap();

    g.insert_vertex(empty_vertex()).unwrap();
    g.insert_vertex(empty_vertex()).unwrap();
    g.sync_pma_meta().unwrap();

    let err = g.insert_directed(0, 1, TE([9, 0, 0, 0])).unwrap_err();
    assert_eq!(
        err,
        CsrGraphError::NeighborMismatch {
            expected: 1,
            actual: 9
        }
    );
}
