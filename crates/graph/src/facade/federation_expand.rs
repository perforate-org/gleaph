//! Shard-local incoming expand for federated graph queries.

use super::stable::REMOTE_FORWARD_IN;
use super::store::{EdgeHandle, GraphStore, GraphStoreError};
use crate::index::placement;
use gleaph_graph_kernel::entry::{Edge, EdgeTarget, RemoteRefId};
use gleaph_graph_kernel::federation::{
    FederatedExpandNeighbor, FederatedIncomingExpandArgs, FederatedOutgoingExpandArgs,
    LocalVertexId, LogicalVertexId, PhysicalVertexLocation, ShardId, VertexPlacement,
};
use ic_stable_lara::traits::{CsrEdgeTombstone, CsrVertexTombstone};
use ic_stable_lara::VertexId;

fn logical_id_for_local_vertex(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Option<LogicalVertexId> {
    store.logical_vertex_id(vertex_id)
}

fn push_neighbor(
    out: &mut Vec<FederatedExpandNeighbor>,
    shard_id: ShardId,
    target_local_vertex_id: LocalVertexId,
    neighbor_logical_vertex_id: LogicalVertexId,
    neighbor_local_vertex_id: LocalVertexId,
    edge: &Edge,
) {
    out.push(FederatedExpandNeighbor {
        shard_id,
        neighbor_logical_vertex_id,
        neighbor_local_vertex_id: u32::from(neighbor_local_vertex_id),
        target_local_vertex_id,
        label_id_raw: edge.label_id,
        slot_index: edge.edge_slot_index.raw(),
        inline_value: edge.inline_value,
    });
}

fn label_matches(edge: &Edge, label_id_raw: Option<u16>) -> bool {
    label_id_raw.is_none_or(|expected| edge.label_id == expected)
}

fn collect_authoritative_incoming(
    store: &GraphStore,
    shard_id: ShardId,
    target_local: VertexId,
    _target_logical: LogicalVertexId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let target_local_raw = placement::local_vertex_id_raw(target_local);
    for edge in store.in_edges(target_local).map_err(GraphStoreError::from)? {
        if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
            continue;
        }
        let owner = store.edge_sidecar_owner_from_in_row(target_local, &edge);
        let Some(neighbor_logical) = logical_id_for_local_vertex(store, owner) else {
            continue;
        };
        push_neighbor(
            out,
            shard_id,
            target_local_raw,
            neighbor_logical,
            placement::local_vertex_id_raw(owner),
            &edge,
        );
    }
    Ok(())
}

fn push_forward_to_remote_hit(
    store: &GraphStore,
    shard_id: ShardId,
    source_vertex_id: VertexId,
    remote_ref: RemoteRefId,
    label_id_raw: Option<u16>,
    label_id: u16,
    slot_index: u32,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let Some(source_logical) = logical_id_for_local_vertex(store, source_vertex_id) else {
        return Ok(());
    };
    let edge = store
        .out_edges(source_vertex_id)
        .map_err(GraphStoreError::from)?
        .into_iter()
        .find(|edge| edge.label_id == label_id && edge.edge_slot_index.raw() == slot_index);
    let Some(edge) = edge else {
        return Ok(());
    };
    if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
        return Ok(());
    }
    let Some(EdgeTarget::Remote(found)) = edge.edge_target() else {
        return Ok(());
    };
    if found != remote_ref {
        return Ok(());
    }
    push_neighbor(
        out,
        shard_id,
        0,
        source_logical,
        placement::local_vertex_id_raw(source_vertex_id),
        &edge,
    );
    Ok(())
}

fn collect_forward_to_remote_incoming_from_index(
    store: &GraphStore,
    shard_id: ShardId,
    remote_ref: RemoteRefId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let keys = REMOTE_FORWARD_IN.with_borrow(|index| {
        let mut keys = Vec::new();
        index.for_each_for_remote_ref(remote_ref, |key| keys.push(key));
        keys
    });
    for key in keys {
        push_forward_to_remote_hit(
            store,
            shard_id,
            key.source_vertex_id(),
            remote_ref,
            label_id_raw,
            key.label_id(),
            key.slot_index(),
            out,
        )?;
    }
    Ok(())
}

fn collect_forward_to_remote_incoming_scan(
    store: &GraphStore,
    shard_id: ShardId,
    remote_ref: RemoteRefId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
    backfill_index: bool,
) -> Result<(), GraphStoreError> {
    let vertex_count = u32::from(store.vertex_count());
    for raw in 0..vertex_count {
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone() {
            continue;
        };
        for edge in store.out_edges(vertex_id).map_err(GraphStoreError::from)? {
            if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
                continue;
            }
            let Some(EdgeTarget::Remote(found)) = edge.edge_target() else {
                continue;
            };
            if found != remote_ref {
                continue;
            }
            if backfill_index {
                store.register_remote_forward_in(
                    EdgeHandle {
                        owner_vertex_id: vertex_id,
                        label_id: ic_stable_lara::BucketLabelKey::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw(),
                    },
                    remote_ref,
                );
            }
            let Some(source_logical) = logical_id_for_local_vertex(store, vertex_id) else {
                continue;
            };
            push_neighbor(
                out,
                shard_id,
                0,
                source_logical,
                placement::local_vertex_id_raw(vertex_id),
                &edge,
            );
        }
    }
    Ok(())
}

fn collect_forward_to_remote_incoming(
    store: &GraphStore,
    shard_id: ShardId,
    _target_logical: LogicalVertexId,
    remote_ref: RemoteRefId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let index_populated = REMOTE_FORWARD_IN.with_borrow(|index| !index.is_empty());
    if index_populated {
        collect_forward_to_remote_incoming_from_index(
            store,
            shard_id,
            remote_ref,
            label_id_raw,
            out,
        )?;
        if !out.is_empty() {
            return Ok(());
        }
    }
    collect_forward_to_remote_incoming_scan(
        store,
        shard_id,
        remote_ref,
        label_id_raw,
        out,
        true,
    )
}

/// Lists incoming neighbors of `target_logical_vertex_id` visible on this graph shard.
pub fn collect_incoming_neighbors(
    store: &GraphStore,
    args: FederatedIncomingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(
                gleaph_graph_kernel::federation::RouterError::ShardNotRegistered,
            ),
        ))?;

    let mut out = Vec::new();
    if let Ok(VertexPlacement::Active(PhysicalVertexLocation {
        shard_id,
        local_vertex_id,
    })) = placement::resolve_placement(
        routing.router_canister,
        args.target_logical_vertex_id,
    ) {
        if shard_id == routing.shard_id {
            collect_authoritative_incoming(
                store,
                routing.shard_id,
                VertexId::from(local_vertex_id),
                args.target_logical_vertex_id,
                args.label_id_raw,
                &mut out,
            )?;
            return Ok(out);
        }
    }

    if let Some(remote_ref) = store.remote_ref_for_logical(args.target_logical_vertex_id) {
        collect_forward_to_remote_incoming(
            store,
            routing.shard_id,
            args.target_logical_vertex_id,
            remote_ref,
            args.label_id_raw,
            &mut out,
        )?;
    }
    Ok(out)
}

fn neighbor_from_out_edge(
    store: &GraphStore,
    edge: &Edge,
) -> Option<(LogicalVertexId, LocalVertexId)> {
    match edge.edge_target()? {
        EdgeTarget::Local(vertex_id) => {
            let logical = logical_id_for_local_vertex(store, vertex_id)?;
            Some((logical, placement::local_vertex_id_raw(vertex_id)))
        }
        EdgeTarget::Remote(remote_ref) => {
            let logical = store.logical_vertex_for_remote_ref(remote_ref)?;
            Some((logical, 0))
        }
    }
}

fn collect_authoritative_outgoing(
    store: &GraphStore,
    shard_id: ShardId,
    source_local: VertexId,
    _source_logical: LogicalVertexId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let source_local_raw = placement::local_vertex_id_raw(source_local);
    for edge in store.out_edges(source_local).map_err(GraphStoreError::from)? {
        if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
            continue;
        }
        let Some((neighbor_logical, neighbor_local)) = neighbor_from_out_edge(store, &edge) else {
            continue;
        };
        push_neighbor(
            out,
            shard_id,
            source_local_raw,
            neighbor_logical,
            neighbor_local,
            &edge,
        );
    }
    Ok(())
}

/// Lists outgoing neighbors of `source_logical_vertex_id` on its authoritative shard.
pub fn collect_outgoing_neighbors(
    store: &GraphStore,
    args: FederatedOutgoingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(
                gleaph_graph_kernel::federation::RouterError::ShardNotRegistered,
            ),
        ))?;

    let mut out = Vec::new();
    let Some(VertexPlacement::Active(PhysicalVertexLocation {
        shard_id,
        local_vertex_id,
    })) = placement::resolve_placement(
        routing.router_canister,
        args.source_logical_vertex_id,
    )
    .ok()
    else {
        return Ok(out);
    };
    if shard_id != routing.shard_id {
        return Ok(out);
    }
    collect_authoritative_outgoing(
        store,
        routing.shard_id,
        VertexId::from(local_vertex_id),
        args.source_logical_vertex_id,
        args.label_id_raw,
        &mut out,
    )?;
    Ok(out)
}

/// Queries the authoritative shard for outgoing expand neighbors.
pub async fn federated_outgoing_expand_authoritative_shard(
    store: &GraphStore,
    args: FederatedOutgoingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(
                gleaph_graph_kernel::federation::RouterError::ShardNotRegistered,
            ),
        ))?;
    let graph_name = store
        .logical_graph_name()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Call(
                "logical_graph_name required for federated expand".into(),
            ),
        ))?;

    let placement = placement::resolve_placement(
        routing.router_canister,
        args.source_logical_vertex_id,
    )
    .map_err(GraphStoreError::from)?;
    let VertexPlacement::Active(PhysicalVertexLocation {
        shard_id: authoritative_shard,
        ..
    }) = placement
    else {
        return Ok(Vec::new());
    };

    if authoritative_shard == routing.shard_id {
        return collect_outgoing_neighbors(store, args);
    }

    let shards = placement::list_shards_for_graph(routing.router_canister, &graph_name)
        .map_err(GraphStoreError::from)?;
    let Some(entry) = shards
        .iter()
        .find(|entry| entry.shard_id == authoritative_shard)
    else {
        return Ok(Vec::new());
    };

    crate::index::federation::federated_outgoing_expand(entry.graph_canister, args)
        .await
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(e.to_string()))
        })
}

/// Fan-out incoming expand to every shard registered for this graph.
pub async fn federated_incoming_expand_all_shards(
    store: &GraphStore,
    args: FederatedIncomingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(
                gleaph_graph_kernel::federation::RouterError::ShardNotRegistered,
            ),
        ))?;
    let graph_name = store
        .logical_graph_name()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Call(
                "logical_graph_name required for federated expand".into(),
            ),
        ))?;

    let shards = placement::list_shards_for_graph(routing.router_canister, &graph_name)
        .map_err(GraphStoreError::from)?;

    let mut merged = Vec::new();
    for entry in shards {
        let hits = if entry.shard_id == routing.shard_id {
            collect_incoming_neighbors(store, args)?
        } else {
            crate::index::federation::federated_incoming_expand(entry.graph_canister, args)
                .await
                .map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        e.to_string(),
                    ))
                })?
        };
        merged.extend(hits);
    }
    Ok(merged)
}

/// Builds a local [`EdgeHandle`] for a federated hit returned from this shard.
pub fn edge_handle_for_federated_hit(hit: &FederatedExpandNeighbor) -> EdgeHandle {
    let owner_vertex_id = if hit.target_local_vertex_id != 0 {
        VertexId::from(hit.target_local_vertex_id)
    } else {
        VertexId::from(hit.neighbor_local_vertex_id)
    };
    EdgeHandle {
        owner_vertex_id,
        label_id: ic_stable_lara::BucketLabelKey::from_raw(hit.label_id_raw),
        slot_index: hit.slot_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::{FederationRouting, GraphStore};
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardRegistryEntry;

    fn register_test_shard(shard_id: u32, graph_name: &str) {
        placement::native_test_register_shard(ShardRegistryEntry {
            shard_id,
            graph_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            logical_graph_name: graph_name.into(),
            registered_at_ns: 0,
        });
    }

    #[test]
    fn authoritative_incoming_lists_local_predecessors() {
        register_test_shard(7, "g");
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let target = store.insert_vertex().expect("target");
        let target_logical = store.logical_vertex_id(target).expect("logical");
        let source = store.insert_vertex().expect("source");
        let source_logical = store.logical_vertex_id(source).expect("logical");
        store
            .insert_directed_edge(source, target, None)
            .expect("edge");

        let hits = collect_incoming_neighbors(
            &store,
            FederatedIncomingExpandArgs {
                target_logical_vertex_id: target_logical,
                label_id_raw: None,
            },
        )
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].neighbor_logical_vertex_id, source_logical);
        assert_eq!(hits[0].target_local_vertex_id, u32::from(target));
    }

    #[test]
    fn forward_to_remote_uses_stable_index_after_insert() {
        register_test_shard(7, "g");
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let remote_logical = 99_002u64;
        let remote_ref = store.ensure_remote_ref(remote_logical);
        store
            .insert_directed_edge_to_logical(source, remote_logical, None)
            .expect("remote edge");

        assert!(REMOTE_FORWARD_IN.with_borrow(|index| index.has_postings_for(remote_ref)));

        let hits = collect_incoming_neighbors(
            &store,
            FederatedIncomingExpandArgs {
                target_logical_vertex_id: remote_logical,
                label_id_raw: None,
            },
        )
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].neighbor_local_vertex_id,
            placement::local_vertex_id_raw(source)
        );
    }

    #[test]
    fn delete_remote_forward_edge_removes_index_posting() {
        register_test_shard(7, "g");
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let remote_logical = 99_003u64;
        let remote_ref = store.ensure_remote_ref(remote_logical);
        let handle = store
            .insert_directed_edge_to_logical(source, remote_logical, None)
            .expect("remote edge");
        assert!(REMOTE_FORWARD_IN.with_borrow(|index| index.has_postings_for(remote_ref)));

        store.delete_edge_by_handle(handle).expect("delete");

        assert!(!REMOTE_FORWARD_IN.with_borrow(|index| index.has_postings_for(remote_ref)));
    }

    #[test]
    fn forward_to_remote_lists_sources_on_non_authoritative_shard() {
        register_test_shard(7, "g");
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let source_logical = store.logical_vertex_id(source).expect("logical");
        let remote_logical = 99_001u64;
        store
            .insert_directed_edge_to_logical(source, remote_logical, None)
            .expect("remote edge");

        let hits = collect_incoming_neighbors(
            &store,
            FederatedIncomingExpandArgs {
                target_logical_vertex_id: remote_logical,
                label_id_raw: None,
            },
        )
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].neighbor_logical_vertex_id, source_logical);
        assert_eq!(hits[0].target_local_vertex_id, 0);
    }

    #[test]
    fn authoritative_outgoing_lists_local_and_remote_targets() {
        register_test_shard(7, "g");
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let source_logical = store.logical_vertex_id(source).expect("logical");
        let target = store.insert_vertex().expect("target");
        let target_logical = store.logical_vertex_id(target).expect("logical");
        let remote_logical = 77_007u64;
        store
            .insert_directed_edge(source, target, None)
            .expect("local edge");
        store
            .insert_directed_edge_to_logical(source, remote_logical, None)
            .expect("remote edge");

        let hits = collect_outgoing_neighbors(
            &store,
            FederatedOutgoingExpandArgs {
                source_logical_vertex_id: source_logical,
                label_id_raw: None,
            },
        )
        .expect("collect");

        assert_eq!(hits.len(), 2);
        let logicals: Vec<_> = hits.iter().map(|hit| hit.neighbor_logical_vertex_id).collect();
        assert!(logicals.contains(&target_logical));
        assert!(logicals.contains(&remote_logical));
        assert!(hits.iter().all(|hit| hit.target_local_vertex_id == u32::from(source)));
    }
}
