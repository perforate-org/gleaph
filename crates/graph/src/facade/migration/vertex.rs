//! Export/import a single vertex during router-coordinated migration.

use super::super::store::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_gql_ic::IcExtensionBinaryDecode;
use crate::index::placement;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, TaggedEdgeLabelId, Vertex};
use gleaph_graph_kernel::federation::{
    ExportedEdgeTarget, ExportedOutEdge, ExportedProperty, ExportedVertex, FinishVertexMigrationArgs,
    LogicalVertexId, VertexPlacement,
};
use gleaph_graph_kernel::federation::RouterError;
use ic_stable_lara::labeled::record::LabeledVertex;
use ic_stable_lara::traits::{CsrEdgeTombstone, CsrVertexTombstone};
use ic_stable_lara::{BucketLabelKey as LaraLabelId, VertexId};

fn catalog_label_from_bucket(bucket: LaraLabelId) -> Option<EdgeLabelId> {
    if bucket == LaraLabelId::UNLABELED_DIRECTED || bucket == LaraLabelId::UNLABELED_UNDIRECTED {
        None
    } else {
        Some(EdgeLabelId::from_raw(bucket.label_index()))
    }
}

fn export_edge_target(
    store: &GraphStore,
    edge: &Edge,
) -> Result<ExportedEdgeTarget, GraphStoreError> {
    match edge.edge_target() {
        Some(gleaph_graph_kernel::entry::EdgeTarget::Local(vid)) => {
            let logical_vertex_id = store.logical_vertex_id(vid).ok_or(
                GraphStoreError::VertexPlacement(placement::VertexPlacementError::Rejected(
                    RouterError::VertexNotFound,
                )),
            )?;
            Ok(ExportedEdgeTarget::Local {
                logical_vertex_id,
            })
        }
        Some(gleaph_graph_kernel::entry::EdgeTarget::Remote(remote_ref)) => {
            let logical_vertex_id = store.logical_vertex_for_remote_ref(remote_ref).ok_or(
                GraphStoreError::VertexPlacement(placement::VertexPlacementError::Rejected(
                    RouterError::VertexNotFound,
                )),
            )?;
            Ok(ExportedEdgeTarget::Remote {
                logical_vertex_id,
            })
        }
        None => Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidArgument(
                "edge without target".into(),
            )),
        )),
    }
}

fn export_out_edge(
    store: &GraphStore,
    owner_vertex_id: VertexId,
    edge: &Edge,
) -> Result<ExportedOutEdge, GraphStoreError> {
    let bucket = store
        .find_forward_edge_bucket_label(owner_vertex_id, edge)?
        .unwrap_or(LaraLabelId::UNLABELED_DIRECTED);
    let undirected = TaggedEdgeLabelId::from_raw(bucket.raw()).is_undirected();
    let catalog_label = catalog_label_from_bucket(bucket);

    let handle = EdgeHandle {
        owner_vertex_id,
        label_id: bucket,
        slot_index: edge.edge_slot_index.raw(),
    };
    let properties = store
        .edge_properties(handle)
        .into_iter()
        .map(|(property_id, value)| {
            Ok(ExportedProperty {
                property_id,
                value_bytes: value.to_binary_bytes().map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property encode: {e}"),
                    ))
                })?,
            })
        })
        .collect::<Result<Vec<_>, GraphStoreError>>()?;

    Ok(ExportedOutEdge {
        catalog_label,
        undirected,
        inline_value: edge.inline_value,
        target: export_edge_target(store, edge)?,
        properties,
    })
}

/// Serializes a migrating vertex on the source shard (requires `VertexPlacement::Migrating`).
pub fn export_local_vertex_for_migration(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<ExportedVertex, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let vertex = store
        .vertex(vertex_id)
        .filter(|v| !v.is_tombstone())
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
        ))?;

    let logical_vertex_id = store.logical_vertex_id(vertex_id).ok_or(
        GraphStoreError::VertexPlacement(placement::VertexPlacementError::Rejected(
            RouterError::VertexNotFound,
        )),
    )?;

    let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)?;
    let VertexPlacement::Migrating {
        source,
        destination_shard_id: _,
        ..
    } = placement
    else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotMigrating),
        ));
    };
    if source.shard_id != routing.shard_id
        || source.local_vertex_id != placement::local_vertex_id_raw(vertex_id)
    {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::Forbidden),
        ));
    }

    let mut vertex_row_bytes = vec![0u8; Vertex::BYTES];
    vertex.into_labeled().write_to(&mut vertex_row_bytes);

    let labels = store.vertex_labels(vertex_id, vertex);
    let properties = store
        .vertex_properties(vertex_id)
        .into_iter()
        .map(|(property_id, value)| {
            Ok(ExportedProperty {
                property_id,
                value_bytes: value.to_binary_bytes().map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property encode: {e}"),
                    ))
                })?,
            })
        })
        .collect::<Result<Vec<_>, GraphStoreError>>()?;

    let mut out_edges = Vec::new();
    for edge in store.out_edges(vertex_id).map_err(GraphStoreError::from)? {
        if edge.is_tombstone_edge() {
            continue;
        }
        out_edges.push(export_out_edge(store, vertex_id, &edge)?);
    }

    Ok(ExportedVertex {
        logical_vertex_id,
        source_shard_id: source.shard_id,
        source_local_vertex_id: source.local_vertex_id,
        vertex_row_bytes,
        labels,
        properties,
        out_edges,
    })
}

fn resolve_local_endpoint(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Option<VertexId> {
    let routing = store.federation_routing()?;
    let placement =
        placement::resolve_placement(routing.router_canister, logical_vertex_id).ok()?;
    let VertexPlacement::Active(loc) = placement else {
        return None;
    };
    if loc.shard_id != routing.shard_id {
        return None;
    }
    Some(VertexId::from(loc.local_vertex_id))
}

fn import_out_edge(
    store: &GraphStore,
    owner_vertex_id: VertexId,
    edge: &ExportedOutEdge,
) -> Result<(), GraphStoreError> {
    let logical = match edge.target {
        ExportedEdgeTarget::Local { logical_vertex_id }
        | ExportedEdgeTarget::Remote { logical_vertex_id } => logical_vertex_id,
    };

    let handle = if matches!(edge.target, ExportedEdgeTarget::Remote { .. }) {
        store.insert_directed_edge_to_logical(
            owner_vertex_id,
            logical,
            edge.catalog_label,
        )?
    } else if let Some(target_vertex_id) = resolve_local_endpoint(store, logical) {
        if edge.undirected {
            store.insert_undirected_edge(owner_vertex_id, target_vertex_id, edge.catalog_label)?;
            return Ok(());
        }
        if edge.inline_value == 0 {
            store.insert_directed_edge(
                owner_vertex_id,
                target_vertex_id,
                edge.catalog_label,
            )?
        } else {
            store.insert_directed_edge_with_inline_value(
                owner_vertex_id,
                target_vertex_id,
                edge.catalog_label,
                edge.inline_value,
            )?
        }
    } else {
        return Ok(());
    };

    for prop in &edge.properties {
        let value = Value::from_binary_bytes_with_extensions(
            &prop.value_bytes,
            &IcExtensionBinaryDecode::INSTANCE,
        )
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
                "property decode: {e}"
            )))
        })?;
        store
            .set_edge_property(handle, prop.property_id, value)
            .map_err(GraphStoreError::from)?;
    }
    Ok(())
}

/// Imports vertex data on the destination shard and commits router placement.
pub fn import_migrated_vertex(
    store: &GraphStore,
    bundle: ExportedVertex,
) -> Result<VertexId, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let placement = placement::resolve_placement(routing.router_canister, bundle.logical_vertex_id)?;
    let VertexPlacement::Migrating {
        destination_shard_id,
        ..
    } = placement
    else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotMigrating),
        ));
    };
    if destination_shard_id != routing.shard_id {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::Forbidden),
        ));
    }

    if bundle.vertex_row_bytes.len() != Vertex::BYTES {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidArgument(format!(
                "vertex_row_bytes length {}",
                bundle.vertex_row_bytes.len()
            ))),
        ));
    }

    let labeled = LabeledVertex::read_from(&bundle.vertex_row_bytes);
    let vertex = Vertex::from(labeled);
    let vertex_id = store
        .push_migrated_vertex_row(vertex)
        .map_err(GraphStoreError::from)?;

    store.register_logical_vertex_mapping(vertex_id, bundle.logical_vertex_id);

    let vertex_row = store.vertex(vertex_id).expect("inserted vertex");
    let vertex_row = store
        .set_vertex_labels(vertex_id, vertex_row, bundle.labels)
        .map_err(GraphStoreError::from)?;
    store.set_vertex(vertex_id, vertex_row)?;

    for prop in bundle.properties {
        let value = Value::from_binary_bytes_with_extensions(
            &prop.value_bytes,
            &IcExtensionBinaryDecode::INSTANCE,
        )
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
                "property decode: {e}"
            )))
        })?;
        store
            .set_vertex_property(vertex_id, prop.property_id, value)
            .map_err(GraphStoreError::from)?;
    }

    for edge in &bundle.out_edges {
        import_out_edge(store, vertex_id, edge)?;
    }

    placement::finish_vertex_migration(
        routing.router_canister,
        FinishVertexMigrationArgs {
            logical_vertex_id: bundle.logical_vertex_id,
            destination_local_vertex_id: placement::local_vertex_id_raw(vertex_id),
        },
    )?;

    Ok(vertex_id)
}

/// Tombstones the source physical vertex after destination import completes.
pub fn tombstone_migrated_vertex(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<(), GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let logical_vertex_id = store.logical_vertex_id(vertex_id).ok_or(
        GraphStoreError::VertexPlacement(placement::VertexPlacementError::Rejected(
            RouterError::VertexNotFound,
        )),
    )?;

    let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)?;
    let VertexPlacement::Active(authoritative) = placement else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "expected Active placement after migration".into(),
            )),
        ));
    };
    if authoritative.shard_id == routing.shard_id
        && authoritative.local_vertex_id == placement::local_vertex_id_raw(vertex_id)
    {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "vertex is still authoritative on this shard".into(),
            )),
        ));
    }

    let vertex = store
        .vertex(vertex_id)
        .filter(|v| !v.is_tombstone())
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
        ))?;
    store.set_vertex(vertex_id, vertex.with_tombstone(true))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::mutation_executor::GraphMutationExecutor;
    use crate::facade::{FederationRouting, GraphStore};
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::federation::{
        BeginVertexMigrationArgs, VertexPlacement,
    };
    use gleaph_graph_kernel::federation::PhysicalVertexLocation;

    #[test]
    fn export_import_and_tombstone_roundtrip() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        let source_id = store
            .insert_vertex_named(["MigrateMe"], [("k", Value::Text("v".into()))])
            .expect("insert source");
        let logical = store.logical_vertex_id(source_id).expect("logical");

        placement::begin_vertex_migration(
            Principal::management_canister(),
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: 9,
            },
        )
        .expect("begin");

        let bundle = export_local_vertex_for_migration(&store, source_id).expect("export");
        assert_eq!(bundle.logical_vertex_id, logical);
        assert_eq!(bundle.properties.len(), 1);

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let dest_id = import_migrated_vertex(&store, bundle).expect("import");
        assert_eq!(
            placement::resolve_placement(Principal::management_canister(), logical).expect("place"),
            VertexPlacement::Active(PhysicalVertexLocation::new(9, u32::from(dest_id)))
        );
        assert_eq!(
            store.vertex_property(dest_id, store.property_id("k").expect("pid")),
            Some(Value::Text("v".into()))
        );

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing again");

        tombstone_migrated_vertex(&store, source_id).expect("tombstone");
        assert!(store.vertex(source_id).expect("row").is_tombstone());
    }
}
