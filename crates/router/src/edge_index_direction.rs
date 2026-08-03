//! Edge-index direction mapping owned by Router's logical query/catalog boundary.
//!
//! The shared kernel carries the durable seven-variant direction enum. Router maps GQL's
//! direction vocabulary to that enum and retains the storage-class subset rule used when deciding
//! whether an active index can answer a query. No Router-private numeric direction tags are
//! persisted or projected.

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};
use gleaph_graph_kernel::index::EdgeIndexDirection;

/// Map one logical GQL edge direction to the shared catalog direction enum.
pub const fn to_index_direction(direction: EdgeDirection) -> EdgeIndexDirection {
    match direction {
        EdgeDirection::PointingRight => EdgeIndexDirection::Outgoing,
        EdgeDirection::PointingLeft => EdgeIndexDirection::Incoming,
        EdgeDirection::LeftOrRight => EdgeIndexDirection::OutgoingOrIncoming,
        EdgeDirection::Undirected => EdgeIndexDirection::Undirected,
        EdgeDirection::UndirectedOrRight => EdgeIndexDirection::OutgoingOrUndirected,
        EdgeDirection::LeftOrUndirected => EdgeIndexDirection::IncomingOrUndirected,
        EdgeDirection::AnyDirection => EdgeIndexDirection::Any,
    }
}

/// Return whether an active index direction covers every storage class required by a query.
pub const fn index_applies_to_query(
    index_direction: EdgeIndexDirection,
    query_direction: EdgeDirection,
) -> bool {
    let (query_directed, query_undirected) = storage_classes(query_direction);
    (!query_directed || index_direction.includes_directed())
        && (!query_undirected || index_direction.includes_undirected())
}

const fn storage_classes(direction: EdgeDirection) -> (bool, bool) {
    match direction {
        EdgeDirection::PointingRight | EdgeDirection::PointingLeft | EdgeDirection::LeftOrRight => {
            (true, false)
        }
        EdgeDirection::Undirected => (false, true),
        EdgeDirection::AnyDirection
        | EdgeDirection::UndirectedOrRight
        | EdgeDirection::LeftOrUndirected => (true, true),
    }
}

fn wire_label_for_storage(catalog: EdgeLabelId, directed: bool) -> u16 {
    let directedness = if directed {
        EdgeDirectedness::Directed
    } else {
        EdgeDirectedness::Undirected
    };
    catalog.pack(directedness).raw()
}

/// Expand a query direction into the wire label buckets that must be read from graph-index.
pub fn wire_labels_for_query(catalog: EdgeLabelId, query_direction: EdgeDirection) -> Vec<u16> {
    let (directed, undirected) = storage_classes(query_direction);
    let mut labels = Vec::with_capacity(2);
    if directed {
        labels.push(wire_label_for_storage(catalog, true));
    }
    if undirected {
        labels.push(wire_label_for_storage(catalog, false));
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_index_covers_pointing_right_query() {
        assert!(index_applies_to_query(
            EdgeIndexDirection::Any,
            EdgeDirection::PointingRight,
        ));
    }

    #[test]
    fn pointing_right_index_does_not_cover_any_query() {
        assert!(!index_applies_to_query(
            EdgeIndexDirection::Outgoing,
            EdgeDirection::AnyDirection,
        ));
    }

    #[test]
    fn wire_labels_for_pointing_right_use_directed_bucket() {
        let catalog = EdgeLabelId::from_raw(1);
        let wires = wire_labels_for_query(catalog, EdgeDirection::PointingRight);
        assert_eq!(wires, vec![0x8001]);
    }

    #[test]
    fn wire_labels_for_any_use_both_buckets() {
        let catalog = EdgeLabelId::from_raw(1);
        let wires = wire_labels_for_query(catalog, EdgeDirection::AnyDirection);
        assert_eq!(wires, vec![0x8001, 0x0001]);
    }
}
