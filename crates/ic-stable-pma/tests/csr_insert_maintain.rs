//! `VcsrStores::insert_edge`, DGAP overflow logs inside `M_e`, and `resize_double`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_pma::{
    traits::{CsrEdgeSlot, CsrVertex},
    Bound, StableVec, Storable, VectorMemory, VcsrEdgeStore, VcsrStores,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TE([u8; 4]);

impl CsrEdgeSlot for TE {
    const EDGE_BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self(bytes.try_into().unwrap())
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes.copy_from_slice(&self.0);
    }
}

#[test]
fn insert_maintain_triggers_resize_when_slab_full() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = VcsrEdgeStore::<TE, _>::new(me);
    edges.format_new(2, 1, 8, 0).expect("format");

    vertices.push(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    });

    let stores = VcsrStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get(0).unwrap().degree(), 2);
    let cap_after_two = stores.edges.header().unwrap().elem_capacity;
    assert!(cap_after_two >= 2);

    stores.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get(0).unwrap().degree(), 3);
    assert!(stores.edges.header().unwrap().elem_capacity >= cap_after_two);
}

#[test]
fn overflow_goes_to_log_then_neighborhood_matches_insert_order() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = VcsrEdgeStore::<TE, _>::new(me);
    edges.format_new(32, 1, 8, 0).expect("format");

    // Two vertices: v0 may only use slab slots [0, next_base) = [0, 2); the third out-edge overflows to the log.
    vertices.push(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    });
    vertices.push(&TV {
        slot_base: 2,
        deg: 0,
        log_head: -1,
    });

    let stores = VcsrStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([3, 0, 0, 0])).unwrap();

    let v0 = stores.vertices.get(0).unwrap();
    assert_eq!(v0.degree(), 3);
    assert!(v0.log_head() >= 0, "third edge should use overflow log (slab capped by next vertex base)");

    let neigh = stores
        .edges
        .neighborhood_edges(&stores.vertices, 0)
        .unwrap();
    assert_eq!(neigh.len(), 3);
    assert_eq!(neigh[0].0, [1, 0, 0, 0]);
    assert_eq!(neigh[1].0, [2, 0, 0, 0]);
    assert_eq!(neigh[2].0, [3, 0, 0, 0]);
}

#[test]
fn merge_window_clears_log_head() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = VcsrEdgeStore::<TE, _>::new(me);
    edges.format_new(32, 1, 8, 0).expect("format");

    vertices.push(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    });
    vertices.push(&TV {
        slot_base: 2,
        deg: 0,
        log_head: -1,
    });

    let stores = VcsrStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    assert!(stores.vertices.get(0).unwrap().log_head() >= 0);

    stores
        .edges
        .merge_logs_into_slab_for_window(&stores.vertices, 0, 2)
        .expect("merge");

    let v0 = stores.vertices.get(0).unwrap();
    assert_eq!(v0.log_head(), -1);
    assert_eq!(v0.degree(), 3);
    let neigh = stores
        .edges
        .neighborhood_edges(&stores.vertices, 0)
        .unwrap();
    assert_eq!(neigh.len(), 3);
}

#[test]
fn two_vertices_preallocated_sync_meta() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = VcsrEdgeStore::<TE, _>::new(me);
    edges.format_new(32, 1, 8, 0).expect("format");

    vertices.push(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    });
    vertices.push(&TV {
        slot_base: 4,
        deg: 0,
        log_head: -1,
    });

    let stores = VcsrStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([7, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([8, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get(0).unwrap().degree(), 2);
    assert_eq!(stores.vertices.get(1).unwrap().base_slot_start(), 4);
}
