//! Labeled CSR traversal helpers for query expansion.
//!
//! These helpers resolve the requested label up front and walk a contiguous edge
//! range without per-edge label or direction metadata checks.

use crate::facade::EdgeHandle;
use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{CompactEdge, LabelId, LabelSemantics};
use ic_stable_lara::{
    BidirectionalLabeledError, BidirectionalLabeledLaraGraph, LabelId as LaraLabelId, VertexId,
    traits::CsrEdge,
};
use ic_stable_structures::Memory;

use super::error::PlanQueryError;
use super::executor::EdgeBinding;

/// Store surface required for label-resolved CSR expansion.
pub trait LabeledAdjacencyStore<E: CsrEdge> {
    /// Iterates outgoing edges for one resolved label without per-edge label checks.
    fn for_each_out_edge_for_label<F>(
        &self,
        src: VertexId,
        label_id: LabelId,
        visit: F,
    ) -> Result<(), BidirectionalLabeledError>
    where
        F: FnMut(E);
}

impl<E, M> LabeledAdjacencyStore<E> for BidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn for_each_out_edge_for_label<F>(
        &self,
        src: VertexId,
        label_id: LabelId,
        mut visit: F,
    ) -> Result<(), BidirectionalLabeledError>
    where
        F: FnMut(E),
    {
        let lara_label = LaraLabelId::from_raw(label_id.raw());
        for edge in self.iter_out_edges_for_label(src, lara_label)? {
            visit(edge);
        }
        Ok(())
    }
}

/// Expands one labeled outgoing adjacency list without reading per-edge label metadata.
pub fn for_each_labeled_out_expand_edge<S, F>(
    store: &S,
    src_id: VertexId,
    semantics: LabelSemantics,
    mut visit: F,
) -> Result<(), PlanQueryError>
where
    S: LabeledAdjacencyStore<CompactEdge>,
    F: FnMut(CompactEdge),
{
    if !semantics.directed {
        return Err(PlanQueryError::UnsupportedDirection(
            EdgeDirection::PointingRight,
        ));
    }
    store
        .for_each_out_edge_for_label(src_id, semantics.label_id, |edge| visit(edge))
        .map_err(|_| PlanQueryError::UnsupportedDirection(EdgeDirection::PointingRight))
}

/// Converts one compact labeled edge into the executor binding shape.
pub fn compact_edge_binding(
    owner_vertex_id: VertexId,
    edge: CompactEdge,
) -> (VertexId, EdgeBinding) {
    (
        edge.neighbor_vid(),
        EdgeBinding {
            handle: EdgeHandle {
                owner_vertex_id,
                vertex_edge_id: edge.vertex_edge_id,
            },
            inline_value: edge.inline_value,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::{VertexEdgeId, VertexRef};
    use ic_stable_lara::{BidirectionalLabeledLaraGraph, VertexId};
    use ic_stable_structures::vec_mem::VectorMemory;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn vector_memory() -> Rc<RefCell<Vec<u8>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn labeled_test_graph() -> BidirectionalLabeledLaraGraph<CompactEdge, VectorMemory> {
        let default = LaraLabelId::from_raw(1);
        let graph = BidirectionalLabeledLaraGraph::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            256,
            default,
        )
        .expect("graph");
        for _ in 0..3 {
            graph.push_vertex().expect("vertex");
        }
        graph
    }

    #[test]
    fn labeled_expand_skips_per_edge_label_checks() {
        let graph = labeled_test_graph();
        let road = LabelId::from_raw(2);
        let walk = LabelId::from_raw(3);
        let src = VertexId::from(0);
        let forward = CompactEdge::new(
            VertexRef::local(VertexId::from(1)),
            VertexEdgeId::from_raw(1),
            0,
        );
        let reverse = CompactEdge::new(VertexRef::local(src), VertexEdgeId::from_raw(1), 0);
        graph
            .insert_directed_edge(
                src,
                VertexId::from(1),
                LaraLabelId::from_raw(road.raw()),
                forward,
                reverse,
            )
            .expect("road");
        let forward_walk = CompactEdge::new(
            VertexRef::local(VertexId::from(2)),
            VertexEdgeId::from_raw(2),
            0,
        );
        let reverse_walk = CompactEdge::new(VertexRef::local(src), VertexEdgeId::from_raw(2), 0);
        graph
            .insert_directed_edge(
                src,
                VertexId::from(2),
                LaraLabelId::from_raw(walk.raw()),
                forward_walk,
                reverse_walk,
            )
            .expect("walk");

        let mut road_targets = Vec::new();
        for_each_labeled_out_expand_edge(
            &graph,
            src,
            LabelSemantics::default_directed(road),
            |edge| road_targets.push(edge.neighbor_vid()),
        )
        .expect("expand");
        assert_eq!(road_targets, vec![VertexId::from(1)]);
    }
}
