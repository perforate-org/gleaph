//! Federated expand: shard-local collection and cross-shard coordination.

use super::migration::incremental::{
    forwarding_stub_on_current_shard, migration_visibility_filter_needed,
};
use super::migration::vertex_visible_to_query;
use super::stable::REMOTE_FORWARD_IN;
use super::store::{EdgeHandle, GraphStore, GraphStoreError, canonical_undirected_owner};
use crate::facade::catalog_edge_label_from_wire;
use crate::index::placement;
use gleaph_graph_kernel::entry::{Edge, EdgeTarget, RemoteRefId};
use gleaph_graph_kernel::federation::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor, LocalVertexId,
    LogicalVertexId, PhysicalVertexLocation, ShardId, VertexPlacement,
};
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::{CsrEdge, CsrEdgeTombstone, CsrVertexTombstone};

fn logical_id_for_local_vertex(store: &GraphStore, vertex_id: VertexId) -> Option<LogicalVertexId> {
    store.logical_vertex_id(vertex_id)
}

/// Authoritative physical row for expand while placement is [`VertexPlacement::Active`] or
/// [`VertexPlacement::Migrating`] (outgoing/incoming still served from the source row).
fn authoritative_local_for_expand(placement: VertexPlacement) -> Option<PhysicalVertexLocation> {
    match placement {
        VertexPlacement::Active(loc) => Some(loc),
        VertexPlacement::Migrating { source, .. } => Some(source),
    }
}

fn push_neighbor(
    out: &mut Vec<FederatedExpandNeighbor>,
    shard_id: ShardId,
    anchor_local_vertex_id: LocalVertexId,
    neighbor_logical_vertex_id: LogicalVertexId,
    neighbor_local_vertex_id: LocalVertexId,
    edge: &Edge,
) -> Result<(), GraphStoreError> {
    let neighbor = FederatedExpandNeighbor {
        shard_id,
        neighbor_logical_vertex_id,
        neighbor_local_vertex_id: neighbor_local_vertex_id,
        anchor_local_vertex_id,
        label_id_raw: edge.label_id,
        slot_index: edge.edge_slot_index.raw(),
        payload_bytes: edge.payload.as_slice().to_vec(),
    };
    if let Err(err) = neighbor.validate_wire() {
        return Err(GraphStoreError::FederatedExpandPayload {
            detail: err.to_string(),
        });
    }
    out.push(neighbor);
    Ok(())
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
    for edge in store.directed_in_edges(target_local)? {
        if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
            continue;
        }
        if let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() {
            let Some(neighbor_logical) = store.logical_vertex_for_remote_ref(remote_ref) else {
                continue;
            };
            push_neighbor(out, shard_id, target_local_raw, neighbor_logical, 0, &edge)?;
            continue;
        }
        let owner = store.edge_sidecar_owner_from_in_row(target_local, &edge);
        let Some(neighbor_logical) = logical_id_for_local_vertex(store, owner) else {
            continue;
        };
        let label = ic_stable_lara::BucketLabelKey::from_raw(edge.label_id);
        let forward_edge = store
            .find_outgoing_edge_record(EdgeHandle {
                owner_vertex_id: owner,
                label_id: label,
                slot_index: edge.edge_slot_index.raw(),
            })?
            .unwrap_or(edge);
        push_neighbor(
            out,
            shard_id,
            target_local_raw,
            neighbor_logical,
            placement::local_vertex_id_raw(owner),
            &forward_edge,
        )?;
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
        .directed_out_edges(source_vertex_id)?
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
    )?;
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
    let filter_migration_visibility = migration_visibility_filter_needed();
    for raw in 0..vertex_count {
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone()
            || (filter_migration_visibility && !vertex_visible_to_query(vertex_id))
        {
            continue;
        };
        for edge in store.directed_out_edges(vertex_id)? {
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
            )?;
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
    collect_forward_to_remote_incoming_scan(store, shard_id, remote_ref, label_id_raw, out, true)
}

fn collect_local_forward_to_stub_incoming(
    store: &GraphStore,
    shard_id: ShardId,
    stub_local: VertexId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let vertex_count = u32::from(store.vertex_count());
    let filter_migration_visibility = migration_visibility_filter_needed();
    for raw in 0..vertex_count {
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone()
            || (filter_migration_visibility && !vertex_visible_to_query(vertex_id))
        {
            continue;
        }
        for edge in store.directed_out_edges(vertex_id)? {
            if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
                continue;
            }
            if !matches!(edge.edge_target(), Some(EdgeTarget::Local(v)) if v == stub_local) {
                continue;
            }
            let Some(source_logical) = logical_id_for_local_vertex(store, vertex_id) else {
                continue;
            };
            let anchor = placement::local_vertex_id_raw(stub_local);
            let neighbor = placement::local_vertex_id_raw(vertex_id);
            if out.iter().any(|hit| {
                hit.anchor_local_vertex_id == anchor
                    && hit.neighbor_logical_vertex_id == source_logical
                    && hit.neighbor_local_vertex_id == neighbor
                    && hit.label_id_raw == edge.label_id
                    && hit.slot_index == edge.edge_slot_index.raw()
            }) {
                continue;
            }
            push_neighbor(out, shard_id, anchor, source_logical, neighbor, &edge)?;
        }
    }
    Ok(())
}

/// Shard-local expand: incoming or outgoing neighbors for one logical vertex.
pub async fn collect_federated_expand(
    store: &GraphStore,
    args: FederatedExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    match args.direction {
        FederatedExpandDirection::Incoming => {
            collect_incoming_neighbors(store, args.logical_vertex_id, args.label_id_raw).await
        }
        FederatedExpandDirection::Outgoing => {
            collect_outgoing_neighbors(store, args.logical_vertex_id, args.label_id_raw).await
        }
        FederatedExpandDirection::Undirected => {
            collect_undirected_neighbors(store, args.logical_vertex_id, args.label_id_raw).await
        }
    }
}

/// Lists incoming neighbors of `logical_vertex_id` visible on this graph shard.
async fn collect_incoming_neighbors(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    label_id_raw: Option<u16>,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(
                gleaph_graph_kernel::federation::RouterError::ShardNotRegistered,
            ),
        ))?;

    let mut out = Vec::new();
    if let Ok(placement) =
        placement::resolve_placement(routing.router_canister, logical_vertex_id).await
        && let Some(PhysicalVertexLocation {
            shard_id,
            local_vertex_id,
        }) = authoritative_local_for_expand(placement)
        && shard_id == routing.shard_id
    {
        collect_authoritative_incoming(
            store,
            routing.shard_id,
            VertexId::from(local_vertex_id),
            logical_vertex_id,
            label_id_raw,
            &mut out,
        )?;
        return Ok(out);
    }

    if let Some(stub_local) = forwarding_stub_on_current_shard(store, logical_vertex_id).await {
        let stub_id = VertexId::from(stub_local);
        collect_authoritative_incoming(
            store,
            routing.shard_id,
            stub_id,
            logical_vertex_id,
            label_id_raw,
            &mut out,
        )?;
        collect_local_forward_to_stub_incoming(
            store,
            routing.shard_id,
            stub_id,
            label_id_raw,
            &mut out,
        )?;
    }

    if let Some(remote_ref) = store.remote_ref_for_logical(logical_vertex_id) {
        collect_forward_to_remote_incoming(
            store,
            routing.shard_id,
            logical_vertex_id,
            remote_ref,
            label_id_raw,
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

fn forward_undirected_edge_record(
    store: &GraphStore,
    probe_local: VertexId,
    edge: &Edge,
) -> Result<Edge, GraphStoreError> {
    let owner = canonical_undirected_owner(probe_local, edge.neighbor_vid());
    let label = ic_stable_lara::BucketLabelKey::from_raw(edge.label_id);
    Ok(store
        .find_outgoing_edge_record(EdgeHandle {
            owner_vertex_id: owner,
            label_id: label,
            slot_index: edge.edge_slot_index.raw(),
        })?
        .unwrap_or_else(|| edge.clone()))
}

fn collect_authoritative_undirected(
    store: &GraphStore,
    shard_id: ShardId,
    probe_local: VertexId,
    _probe_logical: LogicalVertexId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let probe_local_raw = placement::local_vertex_id_raw(probe_local);
    for edge in store.undirected_edges(probe_local)? {
        if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
            continue;
        }
        let Some((neighbor_logical, neighbor_local)) = neighbor_from_out_edge(store, &edge) else {
            continue;
        };
        let forward_edge = forward_undirected_edge_record(store, probe_local, &edge)?;
        push_neighbor(
            out,
            shard_id,
            probe_local_raw,
            neighbor_logical,
            neighbor_local,
            &forward_edge,
        )?;
    }
    Ok(())
}

fn collect_undirected_to_remote(
    store: &GraphStore,
    shard_id: ShardId,
    remote_ref: RemoteRefId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let vertex_count = u32::from(store.vertex_count());
    let filter_migration_visibility = migration_visibility_filter_needed();
    for raw in 0..vertex_count {
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone()
            || (filter_migration_visibility && !vertex_visible_to_query(vertex_id))
        {
            continue;
        }
        for edge in store.undirected_edges(vertex_id)? {
            if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
                continue;
            }
            let Some(EdgeTarget::Remote(found)) = edge.edge_target() else {
                continue;
            };
            if found != remote_ref {
                continue;
            }
            let Some(source_logical) = logical_id_for_local_vertex(store, vertex_id) else {
                continue;
            };
            let forward_edge = forward_undirected_edge_record(store, vertex_id, &edge)?;
            push_neighbor(
                out,
                shard_id,
                0,
                source_logical,
                placement::local_vertex_id_raw(vertex_id),
                &forward_edge,
            )?;
        }
    }
    Ok(())
}

fn collect_local_undirected_to_stub(
    store: &GraphStore,
    shard_id: ShardId,
    stub_local: VertexId,
    label_id_raw: Option<u16>,
    out: &mut Vec<FederatedExpandNeighbor>,
) -> Result<(), GraphStoreError> {
    let vertex_count = u32::from(store.vertex_count());
    let filter_migration_visibility = migration_visibility_filter_needed();
    for raw in 0..vertex_count {
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone()
            || (filter_migration_visibility && !vertex_visible_to_query(vertex_id))
        {
            continue;
        }
        for edge in store.undirected_edges(vertex_id)? {
            if edge.is_tombstone_edge() || !label_matches(&edge, label_id_raw) {
                continue;
            }
            if !matches!(edge.edge_target(), Some(EdgeTarget::Local(v)) if v == stub_local) {
                continue;
            }
            let Some(source_logical) = logical_id_for_local_vertex(store, vertex_id) else {
                continue;
            };
            let forward_edge = forward_undirected_edge_record(store, vertex_id, &edge)?;
            let anchor = placement::local_vertex_id_raw(stub_local);
            let neighbor = placement::local_vertex_id_raw(vertex_id);
            if out.iter().any(|hit| {
                hit.anchor_local_vertex_id == anchor
                    && hit.neighbor_logical_vertex_id == source_logical
                    && hit.neighbor_local_vertex_id == neighbor
                    && hit.label_id_raw == forward_edge.label_id
                    && hit.slot_index == forward_edge.edge_slot_index.raw()
            }) {
                continue;
            }
            push_neighbor(
                out,
                shard_id,
                anchor,
                source_logical,
                neighbor,
                &forward_edge,
            )?;
        }
    }
    Ok(())
}

/// Lists undirected neighbors of `logical_vertex_id` visible on this graph shard.
async fn collect_undirected_neighbors(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    label_id_raw: Option<u16>,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(
                gleaph_graph_kernel::federation::RouterError::ShardNotRegistered,
            ),
        ))?;

    let mut out = Vec::new();
    if let Ok(placement) =
        placement::resolve_placement(routing.router_canister, logical_vertex_id).await
        && let Some(PhysicalVertexLocation {
            shard_id,
            local_vertex_id,
        }) = authoritative_local_for_expand(placement)
        && shard_id == routing.shard_id
    {
        collect_authoritative_undirected(
            store,
            routing.shard_id,
            VertexId::from(local_vertex_id),
            logical_vertex_id,
            label_id_raw,
            &mut out,
        )?;
        return Ok(out);
    }

    if let Some(stub_local) = forwarding_stub_on_current_shard(store, logical_vertex_id).await {
        let stub_id = VertexId::from(stub_local);
        collect_authoritative_undirected(
            store,
            routing.shard_id,
            stub_id,
            logical_vertex_id,
            label_id_raw,
            &mut out,
        )?;
        collect_local_undirected_to_stub(store, routing.shard_id, stub_id, label_id_raw, &mut out)?;
    }

    if let Some(remote_ref) = store.remote_ref_for_logical(logical_vertex_id) {
        collect_undirected_to_remote(store, routing.shard_id, remote_ref, label_id_raw, &mut out)?;
    }
    Ok(out)
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
    for edge in store.directed_out_edges(source_local)? {
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
        )?;
    }
    Ok(())
}

/// Lists outgoing neighbors of `logical_vertex_id` on its authoritative shard.
async fn collect_outgoing_neighbors(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    label_id_raw: Option<u16>,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(
                gleaph_graph_kernel::federation::RouterError::ShardNotRegistered,
            ),
        ))?;

    let mut out = Vec::new();
    let Some(placement) = placement::resolve_placement(routing.router_canister, logical_vertex_id)
        .await
        .ok()
    else {
        return Ok(out);
    };
    if let Some(PhysicalVertexLocation {
        shard_id,
        local_vertex_id,
    }) = authoritative_local_for_expand(placement)
    {
        if shard_id == routing.shard_id {
            collect_authoritative_outgoing(
                store,
                routing.shard_id,
                VertexId::from(local_vertex_id),
                logical_vertex_id,
                label_id_raw,
                &mut out,
            )?;
            return Ok(out);
        }
    }

    if let Some(stub_local) = forwarding_stub_on_current_shard(store, logical_vertex_id).await {
        collect_authoritative_outgoing(
            store,
            routing.shard_id,
            VertexId::from(stub_local),
            logical_vertex_id,
            label_id_raw,
            &mut out,
        )?;
    }
    Ok(out)
}

/// Cross-shard expand orchestration used by the query executor (not a canister endpoint).
pub async fn federated_expand_coordinator(
    store: &GraphStore,
    args: FederatedExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, GraphStoreError> {
    match args.direction {
        FederatedExpandDirection::Incoming => {
            federated_expand_incoming_all_shards(store, args).await
        }
        FederatedExpandDirection::Outgoing => {
            federated_expand_outgoing_authoritative(store, args).await
        }
        FederatedExpandDirection::Undirected => {
            federated_expand_undirected_all_shards(store, args).await
        }
    }
}

fn validate_federated_expand_hits(
    store: &GraphStore,
    args: FederatedExpandArgs,
    hits: &[FederatedExpandNeighbor],
) -> Result<(), GraphStoreError> {
    let expected_width = args
        .label_id_raw
        .and_then(|raw| catalog_edge_label_from_wire(ic_stable_lara::BucketLabelKey::from_raw(raw)))
        .and_then(|label| store.edge_label_payload_profile(label))
        .map(|profile| usize::from(profile.required_byte_width()));

    for hit in hits {
        hit.validate_wire()
            .map_err(|err| GraphStoreError::FederatedExpandPayload {
                detail: err.to_string(),
            })?;
        if let Some(label_id_raw) = args.label_id_raw
            && hit.label_id_raw != label_id_raw
        {
            return Err(GraphStoreError::FederatedExpandPayload {
                detail: format!(
                    "requested label {label_id_raw}, remote hit returned label {}",
                    hit.label_id_raw
                ),
            });
        }
        if let Some(expected) = expected_width
            && hit.payload_bytes.len() != expected
        {
            return Err(GraphStoreError::FederatedExpandPayload {
                detail: format!(
                    "label {} expects {expected} value bytes, remote hit returned {}",
                    hit.label_id_raw,
                    hit.payload_bytes.len()
                ),
            });
        }
    }
    Ok(())
}

async fn federated_expand_undirected_all_shards(
    store: &GraphStore,
    args: FederatedExpandArgs,
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
        .await
        .map_err(GraphStoreError::from)?;

    let mut merged = Vec::new();
    for entry in shards {
        let hits = if entry.shard_id == routing.shard_id {
            collect_federated_expand(store, args).await?
        } else {
            crate::index::federation::call_graph_federated_expand(entry.graph_canister, args)
                .await
                .map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        e.to_string(),
                    ))
                })?
        };
        validate_federated_expand_hits(store, args, &hits)?;
        merged.extend(hits);
    }
    Ok(merged)
}

async fn federated_expand_outgoing_authoritative(
    store: &GraphStore,
    args: FederatedExpandArgs,
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

    let placement = placement::resolve_placement(routing.router_canister, args.logical_vertex_id)
        .await
        .map_err(GraphStoreError::from)?;
    let Some(PhysicalVertexLocation {
        shard_id: authoritative_shard,
        ..
    }) = authoritative_local_for_expand(placement)
    else {
        return Ok(Vec::new());
    };

    if authoritative_shard == routing.shard_id {
        return collect_outgoing_neighbors(store, args.logical_vertex_id, args.label_id_raw).await;
    }

    let shards = placement::list_shards_for_graph(routing.router_canister, &graph_name)
        .await
        .map_err(GraphStoreError::from)?;
    let Some(entry) = shards
        .iter()
        .find(|entry| entry.shard_id == authoritative_shard)
    else {
        return Ok(Vec::new());
    };

    let hits = crate::index::federation::call_graph_federated_expand(entry.graph_canister, args)
        .await
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(e.to_string()))
        })?;
    validate_federated_expand_hits(store, args, &hits)?;
    Ok(hits)
}

async fn federated_expand_incoming_all_shards(
    store: &GraphStore,
    args: FederatedExpandArgs,
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
        .await
        .map_err(GraphStoreError::from)?;

    let mut merged = Vec::new();
    for entry in shards {
        let hits = if entry.shard_id == routing.shard_id {
            collect_federated_expand(store, args).await?
        } else {
            crate::index::federation::call_graph_federated_expand(entry.graph_canister, args)
                .await
                .map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        e.to_string(),
                    ))
                })?
        };
        validate_federated_expand_hits(store, args, &hits)?;
        merged.extend(hits);
    }
    Ok(merged)
}

/// Builds an [`EdgeHandle`] from federated wire fields without touching local CSR.
///
/// Use for hits from other shards so local [`VertexId`] values are not mistaken for this shard's
/// vertices during handle resolution.
pub fn edge_handle_from_federated_hit_wire(hit: &FederatedExpandNeighbor) -> EdgeHandle {
    let label_id = ic_stable_lara::BucketLabelKey::from_raw(hit.label_id_raw);
    let owner_vertex_id = if hit.anchor_local_vertex_id == 0 {
        VertexId::from(hit.neighbor_local_vertex_id)
    } else {
        VertexId::from(hit.anchor_local_vertex_id)
    };
    EdgeHandle {
        owner_vertex_id,
        label_id,
        slot_index: hit.slot_index,
    }
}

/// Builds a local [`EdgeHandle`] on the forward CSR owner for a federated hit.
///
/// Outgoing hits store values on the probe vertex (`anchor`); incoming hits store values on the
/// predecessor (`neighbor`) because reverse CSR rows omit payloads.
pub fn edge_handle_for_federated_hit(
    store: &GraphStore,
    hit: &FederatedExpandNeighbor,
) -> Result<EdgeHandle, GraphStoreError> {
    let wire = edge_handle_from_federated_hit_wire(hit);
    let anchor = VertexId::from(hit.anchor_local_vertex_id);
    let neighbor = VertexId::from(hit.neighbor_local_vertex_id);
    if hit.anchor_local_vertex_id == 0 {
        return Ok(wire);
    }
    if store.vertex(anchor).is_some() {
        let at_anchor = EdgeHandle {
            owner_vertex_id: anchor,
            label_id: wire.label_id,
            slot_index: wire.slot_index,
        };
        if store
            .find_outgoing_edge_record(at_anchor)?
            .is_some_and(|edge| edge.neighbor_vid() == neighbor)
        {
            return Ok(at_anchor);
        }
        if store.vertex(neighbor).is_some() {
            let at_neighbor = EdgeHandle {
                owner_vertex_id: neighbor,
                label_id: wire.label_id,
                slot_index: wire.slot_index,
            };
            if store
                .find_outgoing_edge_record(at_neighbor)?
                .is_some_and(|edge| edge.neighbor_vid() == anchor)
            {
                return Ok(at_neighbor);
            }
            let undirected_owner = canonical_undirected_owner(anchor, neighbor);
            return Ok(EdgeHandle {
                owner_vertex_id: undirected_owner,
                label_id: wire.label_id,
                slot_index: wire.slot_index,
            });
        }
    }
    Ok(wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::{FederationRouting, GraphStore};
    use candid::Principal;
    use gleaph_graph_kernel::entry::{
        EdgePayload, EdgePayloadEncoding, EdgePayloadProfile, EdgeSlotIndex,
    };
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
    fn authoritative_incoming_includes_edge_payload_bytes() {
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
        let label_id = store
            .get_or_insert_edge_label_id("FedIncomingValue")
            .expect("label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                gleaph_graph_kernel::entry::EdgeWeightProfile {
                    encoding: gleaph_graph_kernel::entry::WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[7, 0])
            .expect("edge");

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: target_logical,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
        ))
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload_len(), 2);
        assert_eq!(hits[0].payload_bytes[..2], [7, 0]);
        let handle = edge_handle_for_federated_hit(&store, &hits[0]).expect("handle");
        assert_eq!(
            u32::from(handle.owner_vertex_id),
            u32::from(source),
            "incoming forward owner is the predecessor"
        );
    }

    #[test]
    fn push_neighbor_rejects_oversize_payload_bytes() {
        let edge = Edge {
            target: gleaph_graph_kernel::entry::VertexRef::local(VertexId::from(1)),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 1,
            payload: EdgePayload::from_slice(&vec![
                0;
                usize::from(
                    gleaph_graph_kernel::federation::MAX_FEDERATED_EXPAND_PAYLOAD_BYTE_WIDTH
                ) + 1
            ]),
        };
        let err = push_neighbor(&mut Vec::new(), 0, 0, 1, 1, &edge).unwrap_err();
        assert!(matches!(
            err,
            GraphStoreError::FederatedExpandPayload { .. }
        ));
    }

    #[test]
    fn remote_hits_must_match_label_edge_payload_width() {
        let store = GraphStore::new();
        let label_id = store
            .get_or_insert_edge_label_id("FedWidthCheck")
            .expect("label");
        store
            .install_edge_label_payload_profile_at_init(
                label_id,
                EdgePayloadProfile {
                    byte_width: 2,
                    encoding: EdgePayloadEncoding::RawU16,
                },
            )
            .expect("profile");
        let hit = FederatedExpandNeighbor {
            shard_id: 1,
            neighbor_logical_vertex_id: 2,
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: label_id.raw(),
            slot_index: 0,
            payload_bytes: vec![9],
        };
        let err = validate_federated_expand_hits(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: 1,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: Some(label_id.raw()),
            },
            &[hit],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            GraphStoreError::FederatedExpandPayload { .. }
        ));
    }

    #[test]
    fn remote_hits_must_match_requested_label() {
        let store = GraphStore::new();
        let requested = store
            .get_or_insert_edge_label_id("FedRequestedLabel")
            .expect("requested label");
        let returned = store
            .get_or_insert_edge_label_id("FedReturnedLabel")
            .expect("returned label");
        let hit = FederatedExpandNeighbor {
            shard_id: 1,
            neighbor_logical_vertex_id: 2,
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: returned.raw(),
            slot_index: 0,
            payload_bytes: Vec::new(),
        };
        let err = validate_federated_expand_hits(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: 1,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: Some(requested.raw()),
            },
            &[hit],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            GraphStoreError::FederatedExpandPayload { .. }
        ));
    }

    #[test]
    fn remote_hits_reject_oversize_payload_bytes_before_merge() {
        let store = GraphStore::new();
        let hit = FederatedExpandNeighbor {
            shard_id: 1,
            neighbor_logical_vertex_id: 2,
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: 0,
            slot_index: 0,
            payload_bytes: vec![
                0;
                usize::from(
                    gleaph_graph_kernel::federation::MAX_FEDERATED_EXPAND_PAYLOAD_BYTE_WIDTH
                ) + 1
            ],
        };
        let err = validate_federated_expand_hits(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: 1,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
            &[hit],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            GraphStoreError::FederatedExpandPayload { .. }
        ));
    }

    #[test]
    fn authoritative_incoming_includes_remote_predecessor_rows() {
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
        let target_logical = store.logical_vertex_id(target).expect("target logical");
        let remote_logical = 10_001;
        let remote_ref = store.ensure_remote_ref(remote_logical);
        let reverse = gleaph_graph_kernel::entry::Edge {
            target: gleaph_graph_kernel::entry::VertexRef::remote_ref(remote_ref),
            edge_slot_index: gleaph_graph_kernel::entry::EdgeSlotIndex::from_raw(0),
            label_id: 0,
            payload: gleaph_graph_kernel::entry::EdgePayload::from_slice(&[3, 0]),
        };
        store
            .with_graph_mut(|graph| {
                graph.reverse().insert_edge(
                    target,
                    ic_stable_lara::BucketLabelKey::UNLABELED_DIRECTED,
                    reverse,
                )
            })
            .expect("insert reverse");

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: target_logical,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
        ))
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].neighbor_logical_vertex_id, remote_logical);
        assert_eq!(hits[0].neighbor_local_vertex_id, 0);
        assert_eq!(hits[0].payload_bytes[..2], [3, 0]);
    }

    #[test]
    fn authoritative_undirected_includes_edge_payload_bytes() {
        use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeWeightProfile, WeightEncoding};

        register_test_shard(7, "g");
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let low = store.insert_vertex().expect("low");
        let low_logical = store.logical_vertex_id(low).expect("logical");
        let high = store.insert_vertex().expect("high");
        let label_id = store
            .get_or_insert_edge_label_id("FedUndirValue")
            .expect("label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_undirected_edge_with_payload_bytes(low, high, Some(label_id), &[5, 0])
            .expect("edge");

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: low_logical,
                direction: FederatedExpandDirection::Undirected,
                label_id_raw: Some(label_id.pack(EdgeDirectedness::Undirected).raw()),
            },
        ))
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload_len(), 2);
        assert_eq!(hits[0].payload_bytes[..2], [5, 0]);
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

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: target_logical,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
        ))
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].neighbor_logical_vertex_id, source_logical);
        assert_eq!(hits[0].anchor_local_vertex_id, u32::from(target));
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

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: remote_logical,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
        ))
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

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: remote_logical,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
        ))
        .expect("collect");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].neighbor_logical_vertex_id, source_logical);
        assert_eq!(hits[0].anchor_local_vertex_id, 0);
    }

    #[test]
    fn outgoing_expand_during_source_migrating() {
        use crate::facade::migration::{migration_staging_begin, migration_start};
        use gleaph_graph_kernel::federation::{BeginVertexMigrationArgs, MigrationStagingArgs};

        register_test_shard(7, "g");
        register_test_shard(9, "g");
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
        let target_logical = store.logical_vertex_id(target).expect("target logical");
        store
            .insert_directed_edge(source, target, None)
            .expect("edge");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: source_logical,
                destination_shard_id: 9,
            },
        ))
        .expect("start");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: source_logical,
                epoch: start.epoch,
                source_shard_id: 7,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: source_logical,
                direction: FederatedExpandDirection::Outgoing,
                label_id_raw: None,
            },
        ))
        .expect("expand");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].neighbor_logical_vertex_id, target_logical);
    }

    #[test]
    fn expand_via_forwarding_stub_after_cutover() {
        use crate::facade::migration::{
            migration_apply_chunk, migration_cutover, migration_maintenance_step_for,
            migration_staging_begin, migration_start, migration_status,
        };
        use gleaph_graph_kernel::federation::{BeginVertexMigrationArgs, MigrationStagingArgs};

        const SOURCE_SHARD: ShardId = 7;
        const DEST_SHARD: ShardId = 9;

        register_test_shard(SOURCE_SHARD, "g");
        register_test_shard(DEST_SHARD, "g");
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: SOURCE_SHARD,
            }))
            .expect("routing");

        let migrant = store.insert_vertex().expect("migrant");
        let migrant_logical = store.logical_vertex_id(migrant).expect("migrant logical");
        let neighbor = store.insert_vertex().expect("neighbor");
        let neighbor_logical = store.logical_vertex_id(neighbor).expect("neighbor logical");
        let peer = store.insert_vertex().expect("peer");
        let peer_logical = store.logical_vertex_id(peer).expect("peer logical");
        store
            .insert_directed_edge(neighbor, migrant, None)
            .expect("edge into migrant");
        store
            .insert_directed_edge(migrant, peer, None)
            .expect("edge out of migrant");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: migrant_logical,
                destination_shard_id: DEST_SHARD,
            },
        ))
        .expect("start");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: DEST_SHARD,
            }))
            .expect("dest routing");
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: migrant_logical,
                epoch: start.epoch,
                source_shard_id: SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(migrant),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        for _ in 0..64 {
            if migration_status(&store, migrant_logical)
                .expect("status")
                .ready_for_cutover
            {
                break;
            }
            store
                .set_federation_routing(Some(FederationRouting {
                    router_canister: Principal::management_canister(),
                    index_canister: Principal::management_canister(),
                    shard_id: SOURCE_SHARD,
                }))
                .expect("source routing");
            if let Some(chunk) =
                pollster::block_on(migration_maintenance_step_for(&store, migrant_logical))
                    .expect("maintenance")
            {
                store
                    .set_federation_routing(Some(FederationRouting {
                        router_canister: Principal::management_canister(),
                        index_canister: Principal::management_canister(),
                        shard_id: DEST_SHARD,
                    }))
                    .expect("dest routing");
                pollster::block_on(migration_apply_chunk(&store, chunk)).expect("apply");
            }
        }

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: DEST_SHARD,
            }))
            .expect("dest routing");
        pollster::block_on(migration_cutover(&store, migrant_logical)).expect("dest cutover");
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: SOURCE_SHARD,
            }))
            .expect("source routing");
        pollster::block_on(migration_cutover(&store, migrant_logical)).expect("source cutover");

        let incoming_on_source = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: migrant_logical,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
        ))
        .expect("incoming on source");
        assert_eq!(incoming_on_source.len(), 1);
        assert_eq!(
            incoming_on_source[0].neighbor_logical_vertex_id,
            neighbor_logical
        );

        let outgoing_on_source = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: migrant_logical,
                direction: FederatedExpandDirection::Outgoing,
                label_id_raw: None,
            },
        ))
        .expect("outgoing on source");
        assert_eq!(outgoing_on_source.len(), 1);
        assert_eq!(
            outgoing_on_source[0].neighbor_logical_vertex_id,
            peer_logical
        );

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: DEST_SHARD,
            }))
            .expect("dest routing");
        let outgoing_on_dest = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: migrant_logical,
                direction: FederatedExpandDirection::Outgoing,
                label_id_raw: None,
            },
        ))
        .expect("outgoing on dest");
        assert!(
            !outgoing_on_dest.is_empty(),
            "authoritative destination shard serves outgoing expand"
        );
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

        let hits = pollster::block_on(collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: source_logical,
                direction: FederatedExpandDirection::Outgoing,
                label_id_raw: None,
            },
        ))
        .expect("collect");

        assert_eq!(hits.len(), 2);
        let logicals: Vec<_> = hits
            .iter()
            .map(|hit| hit.neighbor_logical_vertex_id)
            .collect();
        assert!(logicals.contains(&target_logical));
        assert!(logicals.contains(&remote_logical));
        assert!(
            hits.iter()
                .all(|hit| hit.anchor_local_vertex_id == u32::from(source))
        );
    }
}
