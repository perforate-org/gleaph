//! `DgapStores::insert_edge`, DGAP overflow logs inside `M_e`, and `resize_double`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_pma::{
    traits::{CsrEdgeSlot, CsrVertex},
    Bound, DgapGraphMemories, StableVec, Storable, VectorMemory, DgapEdgeStore, DgapStores,
    DgapStoresError,
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

type TeEdgeStore = DgapEdgeStore<TE, VectorMemory, VectorMemory, VectorMemory>;

fn triple_edge_memories() -> DgapGraphMemories<VectorMemory, VectorMemory, VectorMemory> {
    DgapGraphMemories::new(
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
    )
}

#[test]
fn insert_maintain_triggers_resize_when_slab_full() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
    edges.format_new(2, 1, 8, 0).expect("format");

    vertices.push(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    });

    let stores = DgapStores::new(vertices, edges);
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

    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
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

    let stores = DgapStores::new(vertices, edges);
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

    let from1: Vec<_> = stores
        .edges
        .try_neighborhood_iter_from(&stores.vertices, 0, 1)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(from1.len(), 2);
    assert_eq!(from1[0].0, [2, 0, 0, 0]);
    assert_eq!(from1[1].0, [3, 0, 0, 0]);

    let from2: Vec<_> = stores
        .edges
        .try_neighborhood_iter_from(&stores.vertices, 0, 2)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(from2.len(), 1);
    assert_eq!(from2[0].0, [3, 0, 0, 0]);

    let at_degree: Vec<_> = stores
        .edges
        .try_neighborhood_iter_from(&stores.vertices, 0, 3)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(at_degree.is_empty());

    let clamped: Vec<_> = stores
        .edges
        .try_neighborhood_iter_from(&stores.vertices, 0, 99)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(clamped.is_empty());
}

#[test]
fn merge_window_clears_log_head() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
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

    let stores = DgapStores::new(vertices, edges);
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
fn insert_vertex_twice_then_insert_edges_on_each() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
    edges.format_new(16, 1, 8, 0).expect("format");

    let stores = DgapStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    assert_eq!(stores.edges.slab_append_base_slot(&stores.vertices).unwrap(), 0);
    let id0 = stores
        .insert_vertex(TV {
            slot_base: 999,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    assert_eq!(id0, 0);
    assert_eq!(
        stores.vertices.get(0).unwrap().base_slot_start(),
        0,
        "insert_vertex coerces base_slot_start"
    );

    assert_eq!(
        stores.edges.slab_append_base_slot(&stores.vertices).unwrap(),
        1,
        "second empty tail must not share slab cursor with first"
    );
    let id1 = stores
        .insert_vertex(TV {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    assert_eq!(id1, 1);

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(1, TE([2, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get(0).unwrap().degree(), 1);
    assert_eq!(stores.vertices.get(1).unwrap().degree(), 1);
}

#[test]
fn insert_vertex_rejects_wrong_base() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
    edges.format_new(16, 1, 8, 0).expect("format");

    let stores = DgapStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    let err = stores
        .insert_vertex_strict(TV {
            slot_base: 99,
            deg: 0,
            log_head: -1,
        })
        .unwrap_err();
    assert_eq!(
        err,
        DgapStoresError::Graph(
            "insert_vertex_strict: base_slot_start mismatch (expected slab_append_base_slot)"
        )
    );
}

#[test]
fn insert_vertex_honors_segment_vertex_cap() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
    edges.format_new(32, 1, 2, 0).expect("format");

    let stores = DgapStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    stores
        .insert_vertex(TV {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    stores
        .insert_vertex(TV {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    assert_eq!(stores.vertices.len(), 2);

    let err = stores
        .insert_vertex(TV {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap_err();
    assert_eq!(
        err,
        DgapStoresError::Graph("vertex column cap exceeded (segment_count * segment_size)")
    );
}

#[test]
fn two_vertices_preallocated_sync_meta() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
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

    let stores = DgapStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([7, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([8, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get(0).unwrap().degree(), 2);
    assert_eq!(stores.vertices.get(1).unwrap().base_slot_start(), 4);
}

#[test]
fn write_edge_slab_span_round_trips_two_slots() {
    let edges = TeEdgeStore::new(triple_edge_memories());
    edges.format_new(16, 1, 8, 0).expect("format");
    let h = edges.header().unwrap();
    let mut packed = vec![0u8; 8];
    TE([10, 0, 0, 0]).write_to(&mut packed[0..4]);
    TE([11, 0, 0, 0]).write_to(&mut packed[4..8]);
    edges
        .memories()
        .write_edge_slab_span(h.edge_stride, 3, &packed)
        .expect("span write");
    assert_eq!(edges.read_slot(h.edge_stride, 3), TE([10, 0, 0, 0]));
    assert_eq!(edges.read_slot(h.edge_stride, 4), TE([11, 0, 0, 0]));
}

#[test]
fn insert_edges_matches_sequential_single_vertex_overflow() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
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
    let stores_a = DgapStores::new(vertices, edges);
    stores_a.sync_pma_meta().unwrap();
    stores_a.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores_a.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    stores_a.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    let neigh_a = stores_a
        .edges
        .neighborhood_edges(&stores_a.vertices, 0)
        .unwrap();

    let mv_b: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices_b = StableVec::new(mv_b);
    let edges_b = TeEdgeStore::new(triple_edge_memories());
    edges_b.format_new(32, 1, 8, 0).expect("format");
    vertices_b.push(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    });
    vertices_b.push(&TV {
        slot_base: 2,
        deg: 0,
        log_head: -1,
    });
    let stores_b = DgapStores::new(vertices_b, edges_b);
    stores_b.sync_pma_meta().unwrap();
    stores_b
        .insert_edges([
            (0, TE([1, 0, 0, 0])),
            (0, TE([2, 0, 0, 0])),
            (0, TE([3, 0, 0, 0])),
        ])
        .unwrap();
    let neigh_b = stores_b
        .edges
        .neighborhood_edges(&stores_b.vertices, 0)
        .unwrap();
    assert_eq!(neigh_a, neigh_b);
}

#[test]
fn insert_edges_interleaved_matches_sequential() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = StableVec::new(mv);
    let edges = TeEdgeStore::new(triple_edge_memories());
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
    let stores_a = DgapStores::new(vertices, edges);
    stores_a.sync_pma_meta().unwrap();
    stores_a.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores_a.insert_edge(1, TE([2, 0, 0, 0])).unwrap();
    stores_a.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    let n0_a = stores_a
        .edges
        .neighborhood_edges(&stores_a.vertices, 0)
        .unwrap();
    let n1_a = stores_a
        .edges
        .neighborhood_edges(&stores_a.vertices, 1)
        .unwrap();

    let mv_b: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices_b = StableVec::new(mv_b);
    let edges_b = TeEdgeStore::new(triple_edge_memories());
    edges_b.format_new(32, 1, 8, 0).expect("format");
    vertices_b.push(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    });
    vertices_b.push(&TV {
        slot_base: 4,
        deg: 0,
        log_head: -1,
    });
    let stores_b = DgapStores::new(vertices_b, edges_b);
    stores_b.sync_pma_meta().unwrap();
    stores_b
        .insert_edges([
            (0, TE([1, 0, 0, 0])),
            (1, TE([2, 0, 0, 0])),
            (0, TE([3, 0, 0, 0])),
        ])
        .unwrap();
    assert_eq!(
        n0_a,
        stores_b
            .edges
            .neighborhood_edges(&stores_b.vertices, 0)
            .unwrap()
    );
    assert_eq!(
        n1_a,
        stores_b
            .edges
            .neighborhood_edges(&stores_b.vertices, 1)
            .unwrap()
    );
}
