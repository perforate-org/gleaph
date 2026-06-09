//! Peer expand: cross-shard neighbor discovery during traverse.
//!
//! Wraps `facade::federation_expand` so the executor reaches peers only through the
//! federation module boundary (see `design/sharding/federation-target.md`).

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};
use gleaph_graph_kernel::federation::{FederatedExpandArgs, FederatedExpandNeighbor};

use crate::facade::GraphStore;
use crate::plan::PlanQueryError;

/// Graph ↔ graph peer expand (not a canister endpoint).
pub async fn peer_expand(
    store: &GraphStore,
    args: FederatedExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    crate::facade::federation_expand::federated_expand_coordinator(store, args)
        .await
        .map_err(|e| PlanQueryError::FederatedIndexCall {
            op: "federated_expand",
            detail: e.to_string(),
        })
}

/// Pack edge label + direction for [`FederatedExpandArgs::label_id_raw`].
pub(crate) fn federated_expand_label_id_raw(
    label_id: Option<EdgeLabelId>,
    direction: EdgeDirection,
) -> Option<u16> {
    label_id.map(|lid| {
        let directedness = match direction {
            EdgeDirection::Undirected => EdgeDirectedness::Undirected,
            EdgeDirection::PointingLeft | EdgeDirection::PointingRight => {
                EdgeDirectedness::Directed
            }
            _ => EdgeDirectedness::Directed,
        };
        lid.pack(directedness).raw()
    })
}

#[cfg(test)]
mod tests {
    use gleaph_gql::types::EdgeDirection;

    use super::federated_expand_label_id_raw;
    use crate::test_labels;

    #[test]
    fn federated_expand_label_id_raw_packs_directed_and_undirected() {
        let label_id = test_labels::edge_label_id_for_name("PeerExpandLabel");
        let directed = federated_expand_label_id_raw(Some(label_id), EdgeDirection::PointingRight)
            .expect("directed raw");
        let undirected = federated_expand_label_id_raw(Some(label_id), EdgeDirection::Undirected)
            .expect("undirected raw");
        assert_ne!(directed, undirected);
        assert!(federated_expand_label_id_raw(None, EdgeDirection::PointingRight).is_none());
    }
}
