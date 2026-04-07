//! Gleaph `VertexEntry` / `EdgeEntry` on `ic-stable-csr` DGAP layout (`M_v` / three `M_e` memories).

use std::cell::RefCell;
use std::rc::Rc;

use gleaph_graph_pma::low_level::{
    DGAP_EDGES_AND_LOG_MEMORY_SLOT, DGAP_LOG_MEMORY_SLOT, DGAP_SEGMENT_EDGES_ACTUAL_MEMORY_SLOT,
    DGAP_SEGMENT_EDGES_TOTAL_MEMORY_SLOT, DGAP_VERTEX_MEMORY_SLOT, EMPTY_LOG_OFFSET, EdgeEntry,
    EdgeIndex, EdgeRef, VertexEntry, vertex_entry_for_ic_stable_append,
};
use ic_stable_csr::{
    DgapEdgeStore, DgapGraphMemories, DgapStores, StableVec, VectorMemory,
    layout::dgap::{
        EDGE_REGION_MAGIC, PMA_SEGMENT_EDGES_ACTUAL_MAGIC, PMA_SEGMENT_EDGES_TOTAL_MAGIC,
    },
};

type GleaphEdgeStore = DgapEdgeStore<EdgeEntry, VectorMemory, VectorMemory, VectorMemory>;

#[test]
fn gleaph_types_on_vertex_and_triple_edge_memories() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let m_actual: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let m_total: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let m_edges_log: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv.clone());
    let edges = GleaphEdgeStore::new(DgapGraphMemories::new(
        m_actual.clone(),
        m_total.clone(),
        m_edges_log.clone(),
    ));
    edges.format_new(16, 1, 2, 0).expect("format edge region");

    vertices.push(&VertexEntry::new(
        EdgeIndex::from(EdgeRef::new(0, 0)),
        0,
        EMPTY_LOG_OFFSET,
    ));
    vertices.push(&VertexEntry::new(
        EdgeIndex::from(EdgeRef::new(0, 4)),
        0,
        EMPTY_LOG_OFFSET,
    ));

    let vb = mv.borrow();
    let ab = m_actual.borrow();
    let tb = m_total.borrow();
    let elb = m_edges_log.borrow();
    assert_eq!(&vb[0..3], b"SVC");
    assert_eq!(&ab[0..3], PMA_SEGMENT_EDGES_ACTUAL_MAGIC);
    assert_eq!(&tb[0..3], PMA_SEGMENT_EDGES_TOTAL_MAGIC);
    assert_eq!(&elb[0..3], EDGE_REGION_MAGIC);

    assert_eq!(DGAP_VERTEX_MEMORY_SLOT, 220);
    assert_eq!(DGAP_SEGMENT_EDGES_ACTUAL_MEMORY_SLOT, 221);
    assert_eq!(DGAP_SEGMENT_EDGES_TOTAL_MEMORY_SLOT, 222);
    assert_eq!(DGAP_EDGES_AND_LOG_MEMORY_SLOT, 223);
    assert_eq!(DGAP_LOG_MEMORY_SLOT, 224);
}

#[test]
fn dgap_stores_insert_vertex_with_gleaph_row_helper() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv);
    let edges = GleaphEdgeStore::new(DgapGraphMemories::new(
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
    ));
    edges.format_new(32, 1, 4, 0).expect("format edge region");

    let stores = DgapStores::new(vertices, edges);
    stores.sync_pma_meta().unwrap();

    let segment_size = 4u32;
    let b0 = stores
        .edges
        .slab_append_base_slot(&stores.vertices)
        .unwrap();
    let row0 = vertex_entry_for_ic_stable_append(0, segment_size, b0);
    assert_eq!(row0.segment_id(), 0);
    stores.insert_vertex(row0).unwrap();

    let b1 = stores
        .edges
        .slab_append_base_slot(&stores.vertices)
        .unwrap();
    let row1 = vertex_entry_for_ic_stable_append(1, segment_size, b1);
    assert_eq!(row1.segment_id(), 0);
    stores.insert_vertex(row1).unwrap();

    assert_eq!(stores.vertices.len(), 2);
}
