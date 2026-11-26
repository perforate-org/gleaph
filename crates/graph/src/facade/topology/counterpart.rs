//! An edge's two halves: which is canonical, and where the other one is.
//!
//! A directed edge's canonical half is its forward (source) one; an undirected edge's is the one
//! at [`canonical_undirected_owner`], the larger vertex id. The rule is lara-graph's
//! (`EdgeRef::is_canonical`), and the other half is found through the bucket's target index
//! (`counterpart`), replacing ADR 0048's rank-based scan (ADR 0136, Decision 4). A hole reads as
//! no edge, so a stale handle fails closed.

use gleaph_graph_kernel::entry::{EdgeOrientation, EdgeSlotIndex, TaggedEdgeLabelId, VertexId};

use super::{
    CanonicalEdgeOccurrence, CounterpartLookupError, TopologyError, edge_ref, occurrence_of,
    with_graph,
};

/// The canonical half of an edge: its owner, label and slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalHalf {
    pub owner_vertex_id: VertexId,
    pub label_id: TaggedEdgeLabelId,
    pub slot_index: EdgeSlotIndex,
}

impl From<CanonicalEdgeOccurrence> for CanonicalHalf {
    fn from(occurrence: CanonicalEdgeOccurrence) -> Self {
        Self {
            owner_vertex_id: occurrence.owner_vertex_id,
            label_id: occurrence.label_id,
            slot_index: occurrence.slot_index,
        }
    }
}

/// The endpoint that holds an undirected edge's canonical half: the larger vertex id, as
/// lara-graph's rule has it for two local ends.
pub(crate) fn canonical_undirected_owner(a: VertexId, b: VertexId) -> VertexId {
    if u32::from(a) >= u32::from(b) { a } else { b }
}

/// The canonical half of a resolved pair, with no read: for a directed label the forward half,
/// for an undirected one the half at [`canonical_undirected_owner`].
pub(crate) fn canonical_from_counterpart(
    source: CanonicalEdgeOccurrence,
    counterpart: CanonicalEdgeOccurrence,
) -> CanonicalHalf {
    let canonical = if source.label_id.is_directed() {
        match source.orientation {
            EdgeOrientation::Forward => source,
            EdgeOrientation::Reverse => counterpart,
        }
    } else if canonical_undirected_owner(source.owner_vertex_id, counterpart.owner_vertex_id)
        == source.owner_vertex_id
    {
        source
    } else {
        counterpart
    };
    canonical.into()
}

/// The canonical half of the live edge `occurrence` names.
pub(crate) fn canonical_of(
    occurrence: CanonicalEdgeOccurrence,
) -> Result<CanonicalHalf, CounterpartLookupError> {
    let half = edge_ref(occurrence);
    with_graph(|graph| {
        let map =
            |error| CounterpartLookupError::ReadFailed(TopologyError::from_store(graph, error));
        let Some(target) = graph.edge_target(half).map_err(map)? else {
            return Err(CounterpartLookupError::SourceNotFound(occurrence));
        };
        if half.is_canonical(target) {
            return Ok(occurrence.into());
        }
        let other = graph
            .counterpart(half)
            .map_err(map)?
            .ok_or(CounterpartLookupError::SourceNotFound(occurrence))?;
        Ok(occurrence_of(other).into())
    })
}

/// The other half of the live edge `occurrence` names (itself for an undirected self-loop).
pub(crate) fn counterpart_of(
    occurrence: CanonicalEdgeOccurrence,
) -> Result<CanonicalEdgeOccurrence, CounterpartLookupError> {
    let half = edge_ref(occurrence);
    with_graph(|graph| {
        graph
            .counterpart(half)
            .map_err(|error| {
                CounterpartLookupError::ReadFailed(TopologyError::from_store(graph, error))
            })?
            .map(occurrence_of)
            .ok_or(CounterpartLookupError::SourceNotFound(occurrence))
    })
}
