//! Vertex/index scan operators and limit+offset streaming execution.

use crate::plan::query::row::PlanRow;

mod edge_index;
pub(crate) use edge_index::{EdgeBindEndpoints, EdgeIndexScan};
mod index;
mod streaming;

pub(crate) use edge_index::{execute_edge_bind_endpoints, execute_edge_index_scan};
pub(crate) use index::{
    execute_conditional_index_scan, execute_index_intersection, execute_index_scan,
    execute_node_scan, resolve_edge_equality_probe_payloads,
};
pub(crate) use streaming::{execute_limited_streaming_prefix, limited_streaming_prefix_limit_idx};

/// The vertices a `NodeScan` of `variable` binds on `row`, in id order: live vertices carrying
/// `label_id` (any label when `None`).
///
/// A labeled scan of an unbound variable reads only the label's postings (ADR 0123), so its cost
/// follows the label's cardinality rather than the vertex count. A row that already binds
/// `variable` is checked against that one vertex instead - the scan then only decides whether the
/// bound vertex matches, which is what [`PlanRow::bind`] would leave of a full scan anyway.
pub(super) fn node_scan_vertices<'a>(
    store: &'a crate::facade::GraphStore,
    row: &PlanRow,
    variable: &str,
    label_id: Option<gleaph_graph_kernel::entry::VertexLabelId>,
) -> impl Iterator<Item = gleaph_graph_kernel::entry::VertexId> + 'a {
    use gleaph_graph_kernel::entry::VertexId;
    use ic_stable_lara::traits::CsrVertexTombstone;

    use super::ExpandDst;

    // `label_check` is the label the candidates still have to be checked for: none when they
    // come from that label's postings.
    let (candidates, label_check): (Box<dyn Iterator<Item = VertexId> + 'a>, _) = match (
        row.get(variable).map(super::PlanBinding::vertex_ref),
        label_id,
    ) {
        (None, Some(label)) => (Box::new(store.vertices_with_label(label)), None),
        (None, None) => (
            Box::new((0..u32::from(store.vertex_count())).map(VertexId::from)),
            None,
        ),
        (Some(Some(ExpandDst::Local(id))), label) => (Box::new(std::iter::once(id)), label),
        // Bound to a remote vertex or to a non-vertex value: no local vertex unifies with it.
        (Some(_), _) => (Box::new(std::iter::empty()), None),
    };
    candidates.filter(move |&vertex_id| {
        #[cfg(test)]
        NODE_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
        store
            .vertex(vertex_id)
            .is_some_and(|vertex| !vertex.is_tombstone())
            && label_check.is_none_or(|label| store.vertex_has_label(vertex_id, label))
    })
}

pub(crate) const LIMITED_STREAMING_REMOTE_EXPAND_SOURCE: &str =
    "LimitedStreamingPrefix.remote_expand_source";

pub(crate) struct LimitedStreamingPrefixResult {
    pub(crate) rows: Vec<PlanRow>,
    pub(crate) clears_active_aggregate: bool,
}

#[cfg(test)]
mod test_counters {
    use std::cell::Cell;

    thread_local! {
        pub(crate) static NODE_SCAN_VISITS: Cell<usize> = const { Cell::new(0) };
        pub(crate) static EDGE_STREAM_VISITS: Cell<usize> = const { Cell::new(0) };
    }
}

#[cfg(test)]
pub(crate) use test_counters::{EDGE_STREAM_VISITS, NODE_SCAN_VISITS};

#[cfg(test)]
mod tests;
