//! Integration: `M_v` and `M_e` use independent [`VectorMemory`] backing stores.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_csr::{
    Bound, DgapGraphMemories, DgapStores, Storable, VectorMemory,
    dgap::{DgapEdgeStore, calculate_positions_v1, pma_tree_index},
    layout::dgap::{EDGE_REGION_MAGIC, PMA_SEGMENT_EDGE_COUNTS_MAGIC},
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_slot_map::SlotMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
struct TestVertex {
    slot_base: u64,
    deg: u32,
    log_head: i32,
}

impl CsrVertex for TestVertex {
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

impl Storable for TestVertex {
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
        let b = bytes.as_ref();
        let slot_base = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let deg = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let log_head = i32::from_le_bytes(b[12..16].try_into().unwrap());
        Self {
            slot_base,
            deg,
            log_head,
        }
    }

    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TestEdge([u8; 4]);

impl CsrEdge for TestEdge {
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

type TestEdgeStore = DgapEdgeStore<TestEdge, VectorMemory, VectorMemory>;

fn dual_edge_memories() -> DgapGraphMemories<VectorMemory, VectorMemory> {
    DgapGraphMemories::new(
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
    )
}

#[test]
fn vertex_and_dual_edge_memories_are_isolated_regions() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let m_pma: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let m_edges_log: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv.clone()).unwrap();
    let edges = TestEdgeStore::new(DgapGraphMemories::new(
        m_pma.clone(),
        m_edges_log.clone(),
    ));
    edges.format_new(16, 1, 2, 0).expect("format edge region");

    vertices
        .insert(&TestVertex {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    vertices
        .insert(&TestVertex {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();

    let v_bytes = mv.borrow();
    let p_bytes = m_pma.borrow();
    let el_bytes = m_edges_log.borrow();

    assert_eq!(&v_bytes[0..3], b"SSM", "SlotMap magic on M_v");
    assert_eq!(
        &p_bytes[0..3],
        PMA_SEGMENT_EDGE_COUNTS_MAGIC,
        "PMA segment_edge_counts region magic"
    );
    assert_eq!(
        &el_bytes[0..3],
        EDGE_REGION_MAGIC,
        "VCE graph header on edges+log memory"
    );
}

#[test]
fn calculate_positions_v1_matches_spread() {
    let base = vec![0u64, 4];
    let deg = vec![2u32, 2u32];
    let new_idx = calculate_positions_v1(0, 2, &base, &deg, 4, 4);
    assert!(new_idx[0] <= new_idx[1]);
    assert!(new_idx[1] >= new_idx[0] + deg[0] as u64);
}

#[test]
fn pma_tree_index_single_segment() {
    let i = pma_tree_index(0, 2, 1);
    assert_eq!(i, 1);
}

#[test]
fn dgap_stores_sync_meta() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TestEdgeStore::new(dual_edge_memories());
    edges.format_new(32, 1, 2, 0).unwrap();
    vertices
        .insert(&TestVertex {
            slot_base: 0,
            deg: 1,
            log_head: -1,
        })
        .unwrap();
    vertices
        .insert(&TestVertex {
            slot_base: 4,
            deg: 1,
            log_head: -1,
        })
        .unwrap();

    let stores = DgapStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    let e = stores.edges.read_segment_edge_counts(1).actual;
    assert!(e >= 1);
}
