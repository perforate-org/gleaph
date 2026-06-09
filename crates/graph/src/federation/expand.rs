//! Peer expand: cross-shard neighbor discovery during traverse.
//!
//! Wraps `facade::federation_expand` so the executor reaches peers only through the
//! federation module boundary (see `design/sharding/federation-target.md`).

use gleaph_gql::Value;
use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};
use gleaph_graph_kernel::federation::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor, LogicalVertexId,
    VertexPlacement,
};
use ic_stable_lara::VertexId;

use crate::facade::GraphStore;
use crate::index::placement;
use crate::plan::PlanBinding;
use crate::plan::PlanQueryError;

/// How to execute an expand from a bound traversal source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraversalExpandSource {
    LocalCsr(VertexId),
    PeerExpand(LogicalVertexId),
}

/// Map GQL expand direction to federated expand API direction.
pub(crate) fn federated_direction_for_expand(direction: EdgeDirection) -> FederatedExpandDirection {
    match direction {
        EdgeDirection::PointingLeft => FederatedExpandDirection::Incoming,
        EdgeDirection::PointingRight => FederatedExpandDirection::Outgoing,
        EdgeDirection::Undirected => FederatedExpandDirection::Undirected,
        _ => FederatedExpandDirection::Outgoing,
    }
}

/// Decide whether expand uses local CSR or graph ↔ graph peer expand.
///
/// Index/router seeds bind local [`PlanBinding::Vertex`]; when placement authority lives on
/// another shard, peer expand replaces the legacy `RemoteVertex` index-bind entry path.
pub(crate) async fn resolve_traversal_expand_source(
    store: &GraphStore,
    binding: Option<&PlanBinding>,
    _expand_direction: EdgeDirection,
) -> Result<Option<TraversalExpandSource>, PlanQueryError> {
    let binding = match binding {
        None | Some(PlanBinding::Value(Value::Null)) => return Ok(None),
        Some(binding) => binding,
    };

    let Some(routing) = store.federation_routing() else {
        return match binding {
            PlanBinding::Vertex(vertex_id) => Ok(Some(TraversalExpandSource::LocalCsr(*vertex_id))),
            PlanBinding::RemoteVertex(_) => Err(PlanQueryError::UnsupportedOp(
                "Expand(remote vertex requires federation routing)",
            )),
            _ => Err(PlanQueryError::MissingBinding {
                variable: "expand source".into(),
            }),
        };
    };

    let logical = match binding {
        PlanBinding::Vertex(vertex_id) => store.logical_vertex_id(*vertex_id),
        PlanBinding::RemoteVertex(logical) => Some(*logical),
        _ => {
            return Err(PlanQueryError::MissingBinding {
                variable: "expand source".into(),
            });
        }
    };

    let Some(logical) = logical else {
        return match binding {
            PlanBinding::Vertex(vertex_id) => Ok(Some(TraversalExpandSource::LocalCsr(*vertex_id))),
            PlanBinding::RemoteVertex(_) => unreachable!("RemoteVertex branch sets logical"),
            _ => unreachable!(),
        };
    };

    let placement = placement::resolve_placement(routing.router_canister, logical).await;

    match (binding, placement) {
        (PlanBinding::RemoteVertex(_), Err(_)) => {
            Ok(Some(TraversalExpandSource::PeerExpand(logical)))
        }
        (_, Err(_)) => Err(PlanQueryError::UnsupportedOp(
            "Expand(remote placement lookup)",
        )),
        (_, Ok(VertexPlacement::Active(loc))) if loc.shard_id == routing.shard_id => Ok(Some(
            TraversalExpandSource::LocalCsr(VertexId::from(loc.local_vertex_id)),
        )),
        (_, Ok(VertexPlacement::Active(_))) => {
            Ok(Some(TraversalExpandSource::PeerExpand(logical)))
        }
    }
}

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
