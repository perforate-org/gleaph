//! Inserting an edge: both halves, at the source and target of a directed edge or at both ends of
//! an undirected one, with the inline value its label's width takes.
//!
//! The facade decides placement from the Graph Type and passes it in ([`Placement`], ADR 0136,
//! Decision 5); the label's width is declared the first time a value of it is stored
//! (Decision 7).

use gleaph_graph_kernel::entry::{TaggedEdgeLabelId, VertexId};
use lara_graph::{Edge, EdgeLabel, Width};

use super::counterpart::CanonicalHalf;
use super::{Store, TopologyError, node, occurrence_of, with_graph_mut};

/// Where a label's inserts go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Placement {
    /// After every position the bucket has had: `ORDER BY INSERTION` labels, whose bucket order
    /// is the insertion order.
    Append,
    /// Into a hole when the bucket has one: labels whose order means nothing.
    #[default]
    ReuseHoles,
}

impl Placement {
    const fn store(self) -> lara_graph::Placement {
        match self {
            Self::Append => lara_graph::Placement::Append,
            Self::ReuseHoles => lara_graph::Placement::ReuseHoles,
        }
    }
}

/// The two ends of a new edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeEnds {
    Directed { source: VertexId, target: VertexId },
    Undirected { a: VertexId, b: VertexId },
}

/// Gives `label` inline values of `width` bytes, if it has none yet; a different width already
/// declared fails with [`TopologyError::WidthConflict`]. Directed and undirected edges of a
/// catalog label share its width.
pub(crate) fn ensure_value_width(
    label: TaggedEdgeLabelId,
    width: u16,
) -> Result<(), TopologyError> {
    with_graph_mut(|graph| declare(graph, EdgeLabel::new(label.label_index()), width))
}

fn declare(graph: &mut Store, label: EdgeLabel, width: u16) -> Result<(), TopologyError> {
    let Some(width) = Width::new(width) else {
        return Ok(());
    };
    if graph.width(label) == Some(width) {
        return Ok(());
    }
    graph
        .declare_width(label, width)
        .map_err(|error| TopologyError::from_store(graph, error))
}

/// Inserts an edge of `label` between `ends` carrying `value` in both halves, placed by
/// `placement`, and returns its canonical half. A nonempty `value` first gives the label its
/// width, if it has none.
pub(crate) fn insert_edge(
    ends: EdgeEnds,
    label: TaggedEdgeLabelId,
    value: &[u8],
    placement: Placement,
) -> Result<CanonicalHalf, TopologyError> {
    let store_label = EdgeLabel::new(label.label_index());
    let width = u16::try_from(value.len()).map_err(|_| TopologyError::WrongValueLength {
        label: store_label.get(),
        expected: 0,
        actual: value.len(),
    })?;
    with_graph_mut(|graph| {
        declare(graph, store_label, width)?;
        let edge = match ends {
            EdgeEnds::Directed { source, target } => {
                Edge::directed(node(source), store_label, node(target))
            }
            EdgeEnds::Undirected { a, b } => Edge::undirected(node(a), store_label, node(b)),
        };
        let halves = graph
            .add_edge_with(edge.value(value), placement.store())
            .map_err(|error| TopologyError::from_store(graph, error))?;
        Ok(occurrence_of(halves.canonical()).into())
    })
}
