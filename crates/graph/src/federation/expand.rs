//! Peer expand: cross-shard neighbor discovery during traverse.
//!
//! Cross-shard expand is not implemented; see `design/sharding/federation-target.md`.

use gleaph_gql::Value;
use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};
use gleaph_graph_kernel::federation::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor, GlobalVertexId,
};
use ic_stable_lara::VertexId;

use crate::facade::GraphStore;
use crate::plan::{PlanBinding, PlanQueryError};

/// How to execute an expand from a bound traversal source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraversalExpandSource {
    LocalCsr(VertexId),
}

/// Map GQL expand direction to federated expand API direction.
#[expect(dead_code, reason = "planned cross-shard expand wiring")]
pub(crate) fn federated_direction_for_expand(direction: EdgeDirection) -> FederatedExpandDirection {
    match direction {
        EdgeDirection::PointingLeft => FederatedExpandDirection::Incoming,
        EdgeDirection::PointingRight => FederatedExpandDirection::Outgoing,
        EdgeDirection::Undirected => FederatedExpandDirection::Undirected,
        _ => FederatedExpandDirection::Outgoing,
    }
}

fn resolve_local_expand_vertex(
    store: &GraphStore,
    global: GlobalVertexId,
) -> Result<Option<VertexId>, PlanQueryError> {
    let Some(routing) = store.federation_routing() else {
        return Err(PlanQueryError::UnsupportedOp(
            "Expand(remote vertex requires federation routing)",
        ));
    };
    if global.shard_id != routing.shard_id {
        return Err(PlanQueryError::UnsupportedOp(
            "cross-shard expand (foreign shard authority)",
        ));
    }
    Ok(store.resolve_local_vertex(global))
}

/// Decide whether expand uses local CSR on this shard.
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

    let global = match binding {
        PlanBinding::Vertex(vertex_id) => store.global_vertex_id(*vertex_id),
        PlanBinding::RemoteVertex(global) => Some(*global),
        _ => {
            return Err(PlanQueryError::MissingBinding {
                variable: "expand source".into(),
            });
        }
    };

    let Some(global) = global else {
        return match binding {
            PlanBinding::Vertex(vertex_id) => Ok(Some(TraversalExpandSource::LocalCsr(*vertex_id))),
            PlanBinding::RemoteVertex(_) => unreachable!("RemoteVertex branch sets global"),
            _ => unreachable!(),
        };
    };

    match binding {
        PlanBinding::RemoteVertex(_) if global.shard_id != routing.shard_id => Err(
            PlanQueryError::UnsupportedOp("cross-shard expand (remote vertex binding)"),
        ),
        _ => match resolve_local_expand_vertex(store, global)? {
            Some(local) => Ok(Some(TraversalExpandSource::LocalCsr(local))),
            None => Err(PlanQueryError::UnsupportedOp(
                "Expand(deleted or missing vertex)",
            )),
        },
    }
}

/// Resolve a traversal source to a local CSR [`VertexId`] when this shard is authoritative.
pub(crate) async fn resolve_traversal_expand_local_csr(
    store: &GraphStore,
    binding: Option<&PlanBinding>,
    expand_direction: EdgeDirection,
) -> Result<Option<VertexId>, PlanQueryError> {
    match resolve_traversal_expand_source(store, binding, expand_direction).await? {
        None => Ok(None),
        Some(TraversalExpandSource::LocalCsr(vertex_id)) => Ok(Some(vertex_id)),
    }
}

/// Cross-shard neighbor lookup (not implemented).
#[expect(dead_code, reason = "planned cross-shard expand wiring")]
pub async fn peer_expand(
    _: &GraphStore,
    _: FederatedExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    Err(PlanQueryError::UnsupportedOp(
        "cross-shard federated_expand",
    ))
}

/// Pack edge label + direction for [`FederatedExpandArgs::label_id_raw`].
#[expect(dead_code, reason = "planned cross-shard expand wiring")]
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
