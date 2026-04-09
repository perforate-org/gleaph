#[path = "../common/mod.rs"]
mod common;

use common::{TestEdge as TE, TestVertex as TV, empty_vertex, vm};
use ic_stable_csr::{CsrGraphRowTombstone, VectorMemory};

fn main() {
    let g = CsrGraphRowTombstone::<
        TV,
        TE,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
    >::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 32, 1, 8, 0)
    .unwrap();
    let _ = g.insert_vertex(empty_vertex()).unwrap();
    let _ = g.out_edges_logical(0);
}
