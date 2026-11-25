//! Inserting an edge: both halves, at the source and target of a directed edge or at both ends of
//! an undirected one, with the bucket width its inline values need.
//!
//! The facade decides placement from the Graph Type and passes it in ([`InsertPlacement`]); this
//! module calls the store's insert the facade called before (ADR 0136, Decisions 5 and 11).

use gleaph_graph_kernel::entry::{Edge, TaggedEdgeLabelId, VertexId};
use ic_stable_lara::labeled::bidirectional::EdgeInsertPolicy;

use super::{TopologyError, with_graph_mut};

/// Where a label's inserts go: appended in insertion order, or into a hole when there is one.
pub(crate) use ic_stable_lara::labeled::EdgePlacementPolicy;
/// Where each half of an inserted edge went, when the store knows at once.
pub(crate) use ic_stable_lara::labeled::ScalarInsertPair as InsertedHalves;

/// The two ends of a new edge. A directed edge's first half is its source's, an undirected
/// edge's is `a`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeEnds {
    Directed { source: VertexId, target: VertexId },
    Undirected { a: VertexId, b: VertexId },
}

/// How an insert places its halves.
#[derive(Clone, Copy)]
pub(crate) struct InsertPlacement<'a> {
    /// The inserted label's placement.
    pub(crate) placement: EdgePlacementPolicy,
    /// The placement of any other label the store maintains while it inserts (ic-stable-lara's
    /// deferred compaction); it goes with that store.
    pub(crate) policy_for_label: &'a dyn Fn(TaggedEdgeLabelId) -> EdgePlacementPolicy,
}

/// Gives `label`'s buckets at both `ends` inline values of `width` bytes, if they hold none yet.
pub(crate) fn ensure_value_width(
    ends: EdgeEnds,
    label: TaggedEdgeLabelId,
    width: u16,
) -> Result<(), TopologyError> {
    with_graph_mut(|graph| match ends {
        EdgeEnds::Directed { source, target } => {
            graph.ensure_directed_edge_inline_property_width(source, target, label, width)
        }
        EdgeEnds::Undirected { a, b } => {
            graph.ensure_undirected_edge_inline_property_width(a, b, label, width)
        }
    })
}

/// Inserts an edge of `label` between `ends`: `halves.0` at the first end, `halves.1` at the
/// second. A nonzero `width` first gives both buckets inline values of that width, in the same
/// borrow of the store. Returns where each half went.
pub(crate) fn insert_edge(
    ends: EdgeEnds,
    label: TaggedEdgeLabelId,
    halves: (Edge, Edge),
    width: u16,
    placement: InsertPlacement<'_>,
) -> Result<InsertedHalves, TopologyError> {
    let policy = EdgeInsertPolicy {
        placement: placement.placement,
        policy_for_label: placement.policy_for_label,
    };
    let (first, second) = halves;
    with_graph_mut(|graph| match ends {
        EdgeEnds::Directed { source, target } => {
            if width != 0 {
                graph.ensure_directed_edge_inline_property_width(source, target, label, width)?;
            }
            graph.insert_directed_edge_with_locations(source, target, label, first, second, policy)
        }
        EdgeEnds::Undirected { a, b } => {
            if width != 0 {
                graph.ensure_undirected_edge_inline_property_width(a, b, label, width)?;
            }
            graph.insert_undirected_deferred_with_locations(a, b, label, first, second, policy)
        }
    })
}
