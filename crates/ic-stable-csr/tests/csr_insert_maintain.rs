//! `DgapStores::insert_edge`, DGAP overflow logs inside `M_e`, and `resize_double`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_csr::{
    Bound, CsrEdgeSlotTombstoneScan, DgapEdgeStore, DgapGraphMemories, DgapStores, DgapStoresError,
    SegmentEdgeCounts, Storable, VectorMemory,
    dgap::recount_segment_edge_counts_column,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_slot_map::SlotMap;

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

type TeEdgeStore = DgapEdgeStore<TE, VectorMemory, VectorMemory>;

fn dual_edge_memories() -> DgapGraphMemories<VectorMemory, VectorMemory> {
    DgapGraphMemories::new(
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
    )
}

fn assert_sec_matches_full_recount(
    stores: &DgapStores<TV, TE, VectorMemory, VectorMemory, VectorMemory>,
) {
    let h = stores.edges.header().unwrap();
    let sc = h.segment_count as usize;
    let len = sc * 2;
    let mut buf = vec![
        SegmentEdgeCounts {
            actual: 0,
            total: 0,
            tombstone: 0,
        };
        len
    ];
    let es = h.edge_stride;
    recount_segment_edge_counts_column(
        &stores.vertices,
        stores.vertices.len(),
        h.segment_count,
        h.segment_size,
        h.elem_capacity,
        |slot| {
            let e = stores.edges.read_slot(es, slot);
            TE::record_is_physical_tombstone(&e)
        },
        &mut buf,
    );
    for j in 0..len {
        assert_eq!(
            stores.edges.read_segment_edge_counts(j),
            buf[j],
            "SEC node {j} diverges from full recount"
        );
    }
}

fn assert_slab_tail_matches_column(
    stores: &DgapStores<TV, TE, VectorMemory, VectorMemory, VectorMemory>,
) {
    let h = stores.edges.header().unwrap();
    let n = stores.vertices.len() as usize;
    let mut truth = 0u64;
    for i in 0..n {
        let v = stores.vertices.get_dense(i as u32).unwrap();
        truth = truth.max(v.base_slot_start().saturating_add(v.degree() as u64));
    }
    assert_eq!(
        h.slab_occupied_tail, truth,
        "VCE slab_occupied_tail must equal max(base+degree)"
    );
}

/// Dense order `base_slot_start` must be non-decreasing for the remove-slab suffix split
/// (`first_dense_vertex_base_gt` / binary search on `L`).
fn assert_dense_vertex_bases_non_decreasing(
    stores: &DgapStores<TV, TE, VectorMemory, VectorMemory, VectorMemory>,
) {
    let n = stores.vertices.len() as usize;
    if n < 2 {
        return;
    }
    let mut prev = stores.vertices.get_dense(0).unwrap().base_slot_start();
    for j in 1..n {
        let b = stores.vertices.get_dense(j as u32).unwrap().base_slot_start();
        assert!(
            prev <= b,
            "dense vertex bases must be non-decreasing: base[{}]={} > base[{}]={}",
            j - 1,
            prev,
            j,
            b
        );
        prev = b;
    }
}

#[test]
fn sec_delta_matches_full_recount_after_light_inserts() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(128, 2, 4, 0).expect("format");
    for i in 0..4u64 {
        vertices
            .insert(&TV {
                slot_base: i * 16,
                deg: 0,
                log_head: -1,
            })
            .unwrap();
    }
    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();
    assert_slab_tail_matches_column(&stores);
    assert_sec_matches_full_recount(&stores);

    stores.insert_edge(1, TE([2, 0, 0, 0])).unwrap();
    assert_sec_matches_full_recount(&stores);
    assert_slab_tail_matches_column(&stores);

    stores
        .insert_edges([(2, TE([0, 0, 0, 0])), (2, TE([3, 0, 0, 0]))])
        .unwrap();
    assert_sec_matches_full_recount(&stores);
    assert_slab_tail_matches_column(&stores);
}

#[test]
fn insert_vertex_partial_pma_sync_matches_full_recount() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(256, 4, 4, 0).expect("format");
    for i in 0..4u64 {
        vertices
            .insert(&TV {
                slot_base: i * 8,
                deg: 0,
                log_head: -1,
            })
            .unwrap();
    }
    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    stores
        .insert_vertex(TV {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    assert_sec_matches_full_recount(&stores);
    assert_slab_tail_matches_column(&stores);

    stores
        .insert_vertex(TV {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    assert_sec_matches_full_recount(&stores);
    assert_slab_tail_matches_column(&stores);
}

#[test]
fn insert_maintain_triggers_resize_when_slab_full() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(2, 1, 8, 0).expect("format");

    vertices.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();

    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get_dense(0).unwrap().degree(), 2);
    let cap_after_two = stores.edges.header().unwrap().elem_capacity;
    assert!(cap_after_two >= 2);

    stores.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get_dense(0).unwrap().degree(), 3);
    assert!(stores.edges.header().unwrap().elem_capacity >= cap_after_two);
    assert_slab_tail_matches_column(&stores);
}

#[test]
fn overflow_goes_to_log_then_neighborhood_matches_insert_order() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(32, 1, 8, 0).expect("format");

    // Two vertices: v0 may only use slab slots [0, next_base) = [0, 2); the third out-edge overflows to the log.
    vertices.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();
    vertices.insert(&TV {
        slot_base: 2,
        deg: 0,
        log_head: -1,
    }).unwrap();

    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    assert_slab_tail_matches_column(&stores);

    let v0 = stores.vertices.get_dense(0).unwrap();
    assert_eq!(v0.degree(), 3);
    assert!(
        v0.log_head() >= 0,
        "third edge should use overflow log (slab capped by next vertex base)"
    );

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

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(32, 1, 8, 0).expect("format");

    vertices.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();
    vertices.insert(&TV {
        slot_base: 2,
        deg: 0,
        log_head: -1,
    }).unwrap();

    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    assert!(stores.vertices.get_dense(0).unwrap().log_head() >= 0);

    stores
        .edges
        .merge_logs_into_slab_for_window(&stores.vertices, 0, 2)
        .expect("merge");

    let v0 = stores.vertices.get_dense(0).unwrap();
    assert_eq!(v0.log_head(), -1);
    assert_eq!(v0.degree(), 3);
    let neigh = stores
        .edges
        .neighborhood_edges(&stores.vertices, 0)
        .unwrap();
    assert_eq!(neigh.len(), 3);
    assert_slab_tail_matches_column(&stores);
}

#[test]
fn insert_vertex_twice_then_insert_edges_on_each() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(16, 1, 8, 0).expect("format");

    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    assert_eq!(
        stores
            .edges
            .slab_append_base_slot(&stores.vertices)
            .unwrap(),
        0
    );
    let id0 = stores
        .insert_vertex(TV {
            slot_base: 999,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    assert_eq!(id0, 0);
    assert_eq!(
        stores.vertices.get_dense(0).unwrap().base_slot_start(),
        0,
        "insert_vertex coerces base_slot_start"
    );

    assert_eq!(
        stores
            .edges
            .slab_append_base_slot(&stores.vertices)
            .unwrap(),
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
    assert_eq!(stores.vertices.get_dense(0).unwrap().degree(), 1);
    assert_eq!(stores.vertices.get_dense(1).unwrap().degree(), 1);
    assert_slab_tail_matches_column(&stores);
}

#[test]
fn insert_vertex_rejects_wrong_base() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(16, 1, 8, 0).expect("format");

    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
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

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(32, 1, 2, 0).expect("format");

    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
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
    assert_slab_tail_matches_column(&stores);

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

    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(32, 1, 8, 0).expect("format");

    vertices.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();
    vertices.insert(&TV {
        slot_base: 4,
        deg: 0,
        log_head: -1,
    }).unwrap();

    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    stores.insert_edge(0, TE([7, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([8, 0, 0, 0])).unwrap();
    assert_eq!(stores.vertices.get_dense(0).unwrap().degree(), 2);
    assert_eq!(stores.vertices.get_dense(1).unwrap().base_slot_start(), 4);
    assert_slab_tail_matches_column(&stores);
}

#[test]
fn write_edge_slab_span_round_trips_two_slots() {
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(16, 1, 8, 0).expect("format");
    let h = edges.header().unwrap();
    assert_eq!(h.slab_occupied_tail, 0);
    let mut packed = vec![0u8; 8];
    TE([10, 0, 0, 0]).write_to(&mut packed[0..4]);
    TE([11, 0, 0, 0]).write_to(&mut packed[4..8]);
    edges
        .memories()
        .write_edge_slab_span(h.edge_stride, 3, &packed)
        .expect("span write");
    assert_eq!(edges.read_slot(h.edge_stride, 3), TE([10, 0, 0, 0]));
    assert_eq!(edges.read_slot(h.edge_stride, 4), TE([11, 0, 0, 0]));

    let mut read_back = vec![0u8; 8];
    edges
        .memories()
        .read_edge_slab_span(h.edge_stride, 3, &mut read_back);
    assert_eq!(read_back, packed);
}

#[test]
fn insert_edges_matches_sequential_single_vertex_overflow() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(32, 1, 8, 0).expect("format");
    vertices.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();
    vertices.insert(&TV {
        slot_base: 2,
        deg: 0,
        log_head: -1,
    }).unwrap();
    let stores_a = DgapStores::new(vertices, edges);
    stores_a.refresh_slab_occupied_tail_meta().unwrap();
    stores_a.sync_pma_meta().unwrap();
    stores_a.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores_a.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    stores_a.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    let neigh_a = stores_a
        .edges
        .neighborhood_edges(&stores_a.vertices, 0)
        .unwrap();

    let mv_b: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices_b = SlotMap::new(mv_b).unwrap();
    let edges_b = TeEdgeStore::new(dual_edge_memories());
    edges_b.format_new(32, 1, 8, 0).expect("format");
    vertices_b.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();
    vertices_b.insert(&TV {
        slot_base: 2,
        deg: 0,
        log_head: -1,
    }).unwrap();
    let stores_b = DgapStores::new(vertices_b, edges_b);
    stores_b.refresh_slab_occupied_tail_meta().unwrap();
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
    assert_slab_tail_matches_column(&stores_a);
    assert_slab_tail_matches_column(&stores_b);
}

#[test]
fn insert_edges_interleaved_matches_sequential() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(32, 1, 8, 0).expect("format");
    vertices.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();
    vertices.insert(&TV {
        slot_base: 4,
        deg: 0,
        log_head: -1,
    }).unwrap();
    let stores_a = DgapStores::new(vertices, edges);
    stores_a.refresh_slab_occupied_tail_meta().unwrap();
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
    let vertices_b = SlotMap::new(mv_b).unwrap();
    let edges_b = TeEdgeStore::new(dual_edge_memories());
    edges_b.format_new(32, 1, 8, 0).expect("format");
    vertices_b.insert(&TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }).unwrap();
    vertices_b.insert(&TV {
        slot_base: 4,
        deg: 0,
        log_head: -1,
    }).unwrap();
    let stores_b = DgapStores::new(vertices_b, edges_b);
    stores_b.refresh_slab_occupied_tail_meta().unwrap();
    stores_b.sync_pma_meta().unwrap();
    stores_b
        .insert_edges([
            (0, TE([1, 0, 0, 0])),
            (1, TE([2, 0, 0, 0])),
            (0, TE([3, 0, 0, 0])),
        ])
        .unwrap();
    assert_slab_tail_matches_column(&stores_a);
    assert_slab_tail_matches_column(&stores_b);
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

#[test]
fn remove_slab_physically_preserves_sec_on_chain() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(64, 2, 8, 0).expect("format");
    let template = TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    };
    for _ in 0..8 {
        vertices.insert(&template).unwrap();
    }
    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();
    for i in 0..7 {
        stores
            .insert_edge(i, TE([(i + 1) as u8, 0, 0, 0]))
            .unwrap();
    }
    assert_sec_matches_full_recount(&stores);
    assert_slab_tail_matches_column(&stores);
    stores
        .edges
        .remove_slab_edge_at_local_index_physically(&stores.vertices, 0, 0)
        .expect("remove");
    assert_sec_matches_full_recount(&stores);
    assert_slab_tail_matches_column(&stores);
}

#[test]
fn dense_vertex_bases_non_decreasing_across_mutation_paths() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(64, 2, 8, 0).expect("format");
    let template = TV {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    };
    for _ in 0..8 {
        vertices.insert(&template).unwrap();
    }
    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();
    assert_dense_vertex_bases_non_decreasing(&stores);

    for i in 0..7 {
        stores
            .insert_edge(i, TE([(i + 1) as u8, 0, 0, 0]))
            .unwrap();
        assert_dense_vertex_bases_non_decreasing(&stores);
    }

    stores
        .insert_edges([(3usize, TE([5, 0, 0, 0])), (4, TE([6, 0, 0, 0]))])
        .unwrap();
    assert_dense_vertex_bases_non_decreasing(&stores);

    stores
        .edges
        .merge_logs_into_slab_for_window(&stores.vertices, 0, 8)
        .expect("merge (no-op if empty)");
    assert_dense_vertex_bases_non_decreasing(&stores);

    stores
        .edges
        .remove_slab_edge_at_local_index_physically(&stores.vertices, 0, 0)
        .expect("remove head slab edge");
    assert_dense_vertex_bases_non_decreasing(&stores);

    stores
        .edges
        .remove_slab_edge_at_local_index_physically(&stores.vertices, 5, 0)
        .expect("remove mid slab edge");
    assert_dense_vertex_bases_non_decreasing(&stores);
}

#[test]
fn dense_vertex_bases_non_decreasing_after_resize_double_path() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = SlotMap::new(mv).unwrap();
    let edges = TeEdgeStore::new(dual_edge_memories());
    edges.format_new(2, 1, 8, 0).expect("format");
    vertices
        .insert(&TV {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        })
        .unwrap();
    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();
    assert_dense_vertex_bases_non_decreasing(&stores);

    stores.insert_edge(0, TE([1, 0, 0, 0])).unwrap();
    stores.insert_edge(0, TE([2, 0, 0, 0])).unwrap();
    assert_dense_vertex_bases_non_decreasing(&stores);

    stores.insert_edge(0, TE([3, 0, 0, 0])).unwrap();
    assert_dense_vertex_bases_non_decreasing(&stores);
}
