//! Gleaph `VertexEntry` / `EdgeEntry` on `ic-stable-pma` dual-region layout (`M_v` / `M_e`).

use std::cell::RefCell;
use std::rc::Rc;

use gleaph_graph_pma::low_level::{
    EdgeEntry, EdgeIndex, EdgeRef, VertexEntry, EMPTY_LOG_OFFSET, VCSR_EDGE_MEMORY_SLOT,
    VCSR_VERTEX_MEMORY_SLOT,
};
use ic_stable_pma::{layout::edge_region::EDGE_REGION_MAGIC, StableVec, VectorMemory, VcsrEdgeStore};

#[test]
fn gleaph_types_on_two_independent_memories() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));

    let vertices = StableVec::new(mv.clone());
    let edges = VcsrEdgeStore::<EdgeEntry, _>::new(me.clone());
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
    let eb = me.borrow();
    assert_eq!(&vb[0..3], b"SVC");
    assert_eq!(&eb[0..3], EDGE_REGION_MAGIC);

    assert_eq!(VCSR_VERTEX_MEMORY_SLOT, 220);
    assert_eq!(VCSR_EDGE_MEMORY_SLOT, 221);
}
