//! An edge's two halves: which is canonical, and where the other one is.
//!
//! A directed edge's canonical half is its forward one; an undirected edge's is the one at
//! [`canonical_undirected_owner`]. The rule and the lookup are the store's (ADR 0048's
//! CounterpartScan today, lara-graph's `counterpart` at the backend swap); the facade maps them to
//! its handles and errors (ADR 0136, Decisions 4 and 11).

use super::{CanonicalEdgeOccurrence, CounterpartLookupError, with_graph};

/// The canonical half of an edge: its owner, label and slot.
pub(crate) use ic_stable_lara::labeled::bidirectional::counterpart::EdgeHandle as CanonicalHalf;
/// The canonical half given an occurrence and its counterpart, and the canonical owner of an
/// undirected edge between two vertices: the rule, with no read.
pub(crate) use ic_stable_lara::labeled::bidirectional::counterpart::{
    canonical_from_counterpart, canonical_undirected_owner,
};

/// The canonical half of the edge `occurrence` names.
pub(crate) fn canonical_of(
    occurrence: CanonicalEdgeOccurrence,
) -> Result<CanonicalHalf, CounterpartLookupError> {
    with_graph(|graph| graph.counterparts().canonical_handle(occurrence))
}

/// The other half of the live edge `occurrence` names.
pub(crate) fn counterpart_of(
    occurrence: CanonicalEdgeOccurrence,
) -> Result<CanonicalEdgeOccurrence, CounterpartLookupError> {
    with_graph(|graph| graph.counterparts().of(occurrence))
}
