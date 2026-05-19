//! Typed out-adjacency enumeration for unlabeled [`super::BidirectionalLaraGraph`] stores.

use super::UndirectedEdgeFlag;
use crate::{
    LaraGraph, VertexId,
    labeled::OutEdgeOrder,
    lara::{
        edge::{AscOutEdgesIter, OutEdgesIter},
        operation_error::LaraOperationError,
    },
    traits::CsrEdge,
};
use ic_stable_structures::Memory;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutEdgeDirectednessFilter {
    DirectedOnly,
    UndirectedOnly,
}

#[inline]
fn edge_matches_filter<E: CsrEdge>(edge: &E, filter: OutEdgeDirectednessFilter) -> bool {
    match filter {
        OutEdgeDirectednessFilter::DirectedOnly => {
            !<E as UndirectedEdgeFlag>::marked_undirected(edge)
        }
        OutEdgeDirectednessFilter::UndirectedOnly => {
            <E as UndirectedEdgeFlag>::marked_undirected(edge)
        }
    }
}

pub(crate) fn for_each_lara_out_filtered<E, V, M, Visit>(
    store: &LaraGraph<E, V, M>,
    src: VertexId,
    filter: OutEdgeDirectednessFilter,
    order: OutEdgeOrder,
    visit: &mut Visit,
) -> Result<(), LaraOperationError>
where
    E: CsrEdge,
    V: crate::traits::CsrVertex,
    M: Memory,
    Visit: FnMut(E),
{
    match order {
        OutEdgeOrder::Ascending => {
            for edge in store.asc_out_edges(src)? {
                if edge_matches_filter(&edge, filter) {
                    visit(edge);
                }
            }
        }
        OutEdgeOrder::Descending => {
            store.visit_out_edges(
                src,
                None,
                None,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                |edge| edge_matches_filter(edge, filter),
                |edge| visit(edge),
            )?;
        }
    }
    Ok(())
}

/// Iterator over one unlabeled store row, filtered by edge-payload directedness.
pub enum FilteredOutEdgesIter<'a, E: CsrEdge, M: Memory> {
    Ascending(AscOutEdgesIter<'a, E, M>, OutEdgeDirectednessFilter),
    Descending(OutEdgesIter<'a, E, M>, OutEdgeDirectednessFilter),
}

impl<'a, E, M> Iterator for FilteredOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ascending(iter, filter) => loop {
                let edge = iter.next()?;
                if edge_matches_filter(&edge, *filter) {
                    return Some(edge);
                }
            },
            Self::Descending(iter, filter) => loop {
                let edge = iter.next()?;
                if edge_matches_filter(&edge, *filter) {
                    return Some(edge);
                }
            },
        }
    }
}

pub(crate) fn filtered_out_edges_iter<'a, E, V, M>(
    store: &'a LaraGraph<E, V, M>,
    src: VertexId,
    filter: OutEdgeDirectednessFilter,
    order: OutEdgeOrder,
) -> Result<FilteredOutEdgesIter<'a, E, M>, LaraOperationError>
where
    E: CsrEdge,
    V: crate::traits::CsrVertex,
    M: Memory,
{
    Ok(match order {
        OutEdgeOrder::Ascending => {
            FilteredOutEdgesIter::Ascending(store.asc_out_edges_iter(src)?, filter)
        }
        OutEdgeOrder::Descending => {
            FilteredOutEdgesIter::Descending(store.out_edges_iter(src)?, filter)
        }
    })
}
