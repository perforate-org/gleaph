//! Gleaph `VertexEntry` / `EdgeEntry` on `ic-stable-csr` DGAP layout (`M_v` / two `M_e` memories).

use std::cell::RefCell;
use std::rc::Rc;

use gleaph_graph_pma::low_level::{
    DGAP_EDGES_AND_LOG_MEMORY_SLOT, DGAP_GC_QUEUE_MEMORY_SLOT, DGAP_LOG_MEMORY_SLOT,
    DGAP_SEGMENT_EDGE_COUNTS_MEMORY_SLOT, DGAP_VERTEX_MEMORY_SLOT, EMPTY_LOG_OFFSET, EdgeEntry,
    EdgeIndex, EdgeRef, VertexEntry, vertex_entry_for_ic_stable_append,
};
use ic_stable_csr::{
    DgapEdgeStore, DgapGraphMemories, DgapStores, VectorMemory,
    layout::dgap::{EDGE_REGION_MAGIC, PMA_SEGMENT_EDGE_COUNTS_MAGIC},
};
use ic_stable_slot_map::SlotMap;

type GleaphEdgeStore = DgapEdgeStore<EdgeEntry, VectorMemory, VectorMemory>;

#[test]
fn gleaph_types_on_vertex_and_dual_edge_memories() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let m_pma: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let m_edges_log: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv.clone()).unwrap();
    let edges = GleaphEdgeStore::new(DgapGraphMemories::new(m_pma.clone(), m_edges_log.clone()));
    edges.format_new(16, 1, 2, 0).expect("format edge region");

    vertices
        .insert(&VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(0, 0)),
            0,
            EMPTY_LOG_OFFSET,
        ))
        .unwrap();
    vertices
        .insert(&VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(0, 4)),
            0,
            EMPTY_LOG_OFFSET,
        ))
        .unwrap();

    let vb = mv.borrow();
    let pb = m_pma.borrow();
    let elb = m_edges_log.borrow();
    assert_eq!(&vb[0..3], b"SSM");
    assert_eq!(&pb[0..3], PMA_SEGMENT_EDGE_COUNTS_MAGIC);
    assert_eq!(&elb[0..3], EDGE_REGION_MAGIC);

    // `EdgeEntry: CsrEdgeTombstone` => PMA stride 24; tombstone i64 for node 0 at offset 32..40.
    assert!(pb.len() >= 40);
    assert_eq!(i64::from_le_bytes(pb[32..40].try_into().unwrap()), 0);

    assert_eq!(DGAP_VERTEX_MEMORY_SLOT, 220);
    assert_eq!(DGAP_SEGMENT_EDGE_COUNTS_MEMORY_SLOT, 221);
    assert_eq!(DGAP_EDGES_AND_LOG_MEMORY_SLOT, 222);
    assert_eq!(DGAP_LOG_MEMORY_SLOT, 223);
    assert_eq!(DGAP_GC_QUEUE_MEMORY_SLOT, 224);
}

#[test]
fn dgap_stores_insert_vertex_with_gleaph_row_helper() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = SlotMap::new(mv).unwrap();
    let edges = GleaphEdgeStore::new(DgapGraphMemories::new(
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
