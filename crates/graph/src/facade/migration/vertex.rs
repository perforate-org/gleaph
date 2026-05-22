//! Export/import a single vertex during router-coordinated migration.

use super::super::store::{EdgeHandle, GraphStore, GraphStoreError};
use super::index::{remove_source_index_postings_for_vertex, sync_migration_index_postings};
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement;
use crate::plan::PlanQueryError;
use gleaph_gql::Value;
use gleaph_gql_ic::IcExtensionBinaryDecode;
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, TaggedEdgeLabelId, Vertex};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::federation::{
    ExportedEdgeTarget, ExportedOutEdge, ExportedProperty, ExportedVertex,
    FinishVertexMigrationArgs, VertexPlacement,
};
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
            let logical_vertex_id =
                store
                    .logical_vertex_id(vid)
                    .ok_or(GraphStoreError::VertexPlacement(
                        placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
                    ))?;
            Ok(ExportedEdgeTarget::Local { logical_vertex_id })
        }
        Some(gleaph_graph_kernel::entry::EdgeTarget::Remote(remote_ref)) => {
            let logical_vertex_id = store.logical_vertex_for_remote_ref(remote_ref).ok_or(
                GraphStoreError::VertexPlacement(placement::VertexPlacementError::Rejected(
                    RouterError::VertexNotFound,
                )),
            )?;
            Ok(ExportedEdgeTarget::Remote { logical_vertex_id })
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
        value_bytes: edge.value_bytes().to_vec(),
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

    let logical_vertex_id =
        store
            .logical_vertex_id(vertex_id)
            .ok_or(GraphStoreError::VertexPlacement(
                placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
            ))?;

    let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)?;
    let VertexPlacement::Migrating {
        source,
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
    for edge in store
        .directed_out_edges(vertex_id)?
    {
        if edge.is_tombstone_edge() {
            continue;
        }
        out_edges.push(export_out_edge(store, vertex_id, &edge)?);
    }
    for edge in store
        .undirected_edges(vertex_id)?
    {
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

fn import_out_edge(
    store: &GraphStore,
    owner_vertex_id: VertexId,
    edge: &ExportedOutEdge,
) -> Result<(), GraphStoreError> {
    let handle = store.insert_exported_out_edge(
        owner_vertex_id,
        &edge.target,
        edge.undirected,
        &edge.value_bytes,
        edge.catalog_label,
    )?;

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
    import_migrated_vertex_impl(store, bundle)
}

/// Like [`import_migrated_vertex`], then updates federated index postings for exported properties.
pub async fn import_migrated_vertex_with_index(
    store: &GraphStore,
    bundle: ExportedVertex,
    index: &dyn PropertyIndexLookup,
) -> Result<VertexId, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;
    let dest_shard = routing.shard_id;
    let vertex_id = import_migrated_vertex_impl(store, bundle.clone())?;
    let dest_local = placement::local_vertex_id_raw(vertex_id);
    sync_migration_index_postings(index, &bundle, dest_shard, dest_local)
        .await
        .map_err(plan_query_to_store)?;
    Ok(vertex_id)
}

fn plan_query_to_store(err: PlanQueryError) -> GraphStoreError {
    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(err.to_string()))
}

fn import_migrated_vertex_impl(
    store: &GraphStore,
    bundle: ExportedVertex,
) -> Result<VertexId, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let placement =
        placement::resolve_placement(routing.router_canister, bundle.logical_vertex_id)?;
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

    let _source_vertex_row = LabeledVertex::read_from(&bundle.vertex_row_bytes);
    let vertex = Vertex::default();
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
            .set_vertex_property_without_index_pending(vertex_id, prop.property_id, value)
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
    tombstone_migrated_vertex_impl(store, vertex_id)
}

/// Like [`tombstone_migrated_vertex`], then removes any remaining source-shard index postings.
pub async fn tombstone_migrated_vertex_with_index(
    store: &GraphStore,
    vertex_id: VertexId,
    index: &dyn PropertyIndexLookup,
) -> Result<(), GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;
    let source_local = placement::local_vertex_id_raw(vertex_id);
    remove_source_index_postings_for_vertex(
        index,
        store,
        vertex_id,
        routing.shard_id,
        source_local,
    )
    .await
    .map_err(plan_query_to_store)?;
    tombstone_migrated_vertex_impl(store, vertex_id)
}

fn tombstone_migrated_vertex_impl(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<(), GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let logical_vertex_id =
        store
            .logical_vertex_id(vertex_id)
            .ok_or(GraphStoreError::VertexPlacement(
                placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
            ))?;

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
    use gleaph_graph_kernel::entry::EdgeTarget;
    use gleaph_graph_kernel::federation::PhysicalVertexLocation;
    use gleaph_graph_kernel::federation::{BeginVertexMigrationArgs, VertexPlacement};
    use ic_stable_lara::CsrEdge;

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

    #[derive(Default)]
    struct RecordingIndex {
        removes: std::cell::RefCell<Vec<(u32, u32, u32)>>,
        inserts: std::cell::RefCell<Vec<(u32, u32, u32)>>,
    }

    #[async_trait::async_trait(?Send)]
    impl PropertyIndexLookup for RecordingIndex {
        fn local_shard_id(&self) -> u32 {
            9
        }

        async fn lookup_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<gleaph_graph_kernel::index::PostingHit>, PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_range(
            &self,
            _property_id: u32,
            _req: &gleaph_graph_kernel::index::PostingRangeRequest,
        ) -> Result<Vec<gleaph_graph_kernel::index::PostingHit>, PlanQueryError> {
            Ok(vec![])
        }

        async fn posting_insert_at(
            &self,
            shard_id: u32,
            property_id: u32,
            _value: Vec<u8>,
            vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            self.inserts
                .borrow_mut()
                .push((shard_id, property_id, vertex_id));
            Ok(())
        }

        async fn posting_remove_at(
            &self,
            shard_id: u32,
            property_id: u32,
            _value: Vec<u8>,
            vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            self.removes
                .borrow_mut()
                .push((shard_id, property_id, vertex_id));
            Ok(())
        }
    }

    #[test]
    fn import_syncs_index_postings() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        let source_id = store
            .insert_vertex_named(["Idx"], [("k", Value::Text("v".into()))])
            .expect("insert");
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

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let index = RecordingIndex::default();
        let dest_id = pollster::block_on(import_migrated_vertex_with_index(&store, bundle, &index))
            .expect("import");

        assert_eq!(
            index.removes.borrow().as_slice(),
            &[(
                7,
                store.property_id("k").expect("pid").raw(),
                u32::from(source_id)
            )]
        );
        assert_eq!(
            index.inserts.borrow().as_slice(),
            &[(
                9,
                store.property_id("k").expect("pid").raw(),
                u32::from(dest_id)
            )]
        );
    }

    #[test]
    fn migration_preserves_edge_value_bytes() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("target routing");
        let directed_target = store.insert_vertex().expect("directed target");
        let undirected_target = store.insert_vertex().expect("undirected target");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");
        let source_id = store.insert_vertex().expect("source");
        let directed_label = store
            .get_or_insert_edge_label_id("MigratingDirectedValue")
            .expect("directed label");
        let undirected_label = store
            .get_or_insert_edge_label_id("MigratingUndirectedValue")
            .expect("undirected label");
        store
            .insert_directed_edge_with_value_bytes(
                source_id,
                directed_target,
                Some(directed_label),
                &[1, 2, 3, 4],
            )
            .expect("directed edge");
        store
            .insert_undirected_edge_with_value_bytes(
                source_id,
                undirected_target,
                Some(undirected_label),
                &[9],
            )
            .expect("undirected edge");
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
        assert!(
            bundle
                .out_edges
                .iter()
                .any(|edge| edge.value_bytes == [1, 2, 3, 4])
        );
        assert!(bundle.out_edges.iter().any(|edge| edge.value_bytes == [9]));

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let dest_id = import_migrated_vertex(&store, bundle).expect("import");
        let directed_values: Vec<Vec<u8>> = store
            .directed_out_edges(dest_id)
            .expect("directed out")
            .into_iter()
            .filter(|edge| edge.neighbor_vid() == directed_target)
            .map(|edge| edge.value_bytes().to_vec())
            .collect();
        assert_eq!(directed_values, vec![vec![1, 2, 3, 4]]);

        let undirected_values: Vec<Vec<u8>> = store
            .undirected_edges(dest_id)
            .expect("undirected out")
            .into_iter()
            .filter(|edge| edge.neighbor_vid() == undirected_target)
            .map(|edge| edge.value_bytes().to_vec())
            .collect();
        assert_eq!(undirected_values, vec![vec![9]]);
    }

    #[test]
    fn migration_preserves_edge_to_source_shard_target_as_remote() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        let source_id = store.insert_vertex().expect("source");
        let source_shard_target = store.insert_vertex().expect("source shard target");
        let target_logical = store
            .logical_vertex_id(source_shard_target)
            .expect("target logical");
        let label = store
            .get_or_insert_edge_label_id("MigratingRemoteValue")
            .expect("label");
        store
            .insert_directed_edge_with_value_bytes(
                source_id,
                source_shard_target,
                Some(label),
                &[8],
            )
            .expect("edge");
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
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let dest_id = import_migrated_vertex(&store, bundle).expect("import");
        let edge = store
            .directed_out_edges(dest_id)
            .expect("directed out")
            .into_iter()
            .find(|edge| edge.value_bytes() == [8])
            .expect("remote edge");
        let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
            panic!("expected remote edge target");
        };
        assert_eq!(
            store.logical_vertex_for_remote_ref(remote_ref),
            Some(target_logical)
        );
    }

    #[test]
    fn migration_preserves_remote_undirected_edge_kind() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        let source_id = store.insert_vertex().expect("source");
        let remote_target = store.insert_vertex().expect("remote target");
        let source_logical = store.logical_vertex_id(source_id).expect("source logical");
        let target_logical = store
            .logical_vertex_id(remote_target)
            .expect("target logical");
        let label = store
            .get_or_insert_edge_label_id("MigratingRemoteUndirected")
            .expect("label");

        placement::begin_vertex_migration(
            Principal::management_canister(),
            BeginVertexMigrationArgs {
                logical_vertex_id: source_logical,
                destination_shard_id: 9,
            },
        )
        .expect("begin");

        let mut vertex_row_bytes = vec![0u8; Vertex::BYTES];
        Vertex::default()
            .into_labeled()
            .write_to(&mut vertex_row_bytes);
        let bundle = ExportedVertex {
            logical_vertex_id: source_logical,
            source_shard_id: 7,
            source_local_vertex_id: placement::local_vertex_id_raw(source_id),
            vertex_row_bytes,
            labels: Vec::new(),
            properties: Vec::new(),
            out_edges: vec![ExportedOutEdge {
                catalog_label: Some(label),
                undirected: true,
                value_bytes: vec![7],
                target: ExportedEdgeTarget::Remote {
                    logical_vertex_id: target_logical,
                },
                properties: Vec::new(),
            }],
        };

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let dest_id = import_migrated_vertex(&store, bundle).expect("import");
        assert!(
            store
                .directed_out_edges(dest_id)
                .expect("directed out")
                .is_empty()
        );
        let edge = store
            .undirected_edges(dest_id)
            .expect("undirected out")
            .into_iter()
            .find(|edge| edge.value_bytes() == [7])
            .expect("remote undirected edge");
        let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
            panic!("expected remote edge target");
        };
        assert_eq!(
            store.logical_vertex_for_remote_ref(remote_ref),
            Some(target_logical)
        );
    }

    #[test]
    fn full_migration_roundtrip_with_index_sync() {
        let index = RecordingIndex::default();
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        let source_id = store
            .insert_vertex_named(["Full"], [("k", Value::Text("v".into()))])
            .expect("insert");
        let logical = store.logical_vertex_id(source_id).expect("logical");
        let pid = store.property_id("k").expect("pid").raw();

        placement::begin_vertex_migration(
            Principal::management_canister(),
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: 9,
            },
        )
        .expect("begin");

        let bundle = export_local_vertex_for_migration(&store, source_id).expect("export");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let dest_id = pollster::block_on(import_migrated_vertex_with_index(&store, bundle, &index))
            .expect("import");
        assert_eq!(index.removes.borrow().len(), 1);
        assert_eq!(index.inserts.borrow().len(), 1);

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing again");

        pollster::block_on(tombstone_migrated_vertex_with_index(
            &store, source_id, &index,
        ))
        .expect("tombstone");
        assert!(store.vertex(source_id).expect("row").is_tombstone());
        assert_eq!(
            index.removes.borrow().as_slice(),
            &[
                (7, pid, u32::from(source_id)),
                (7, pid, u32::from(source_id)),
            ]
        );
        assert_eq!(
            index.inserts.borrow().as_slice(),
            &[(9, pid, u32::from(dest_id))]
        );
    }

    #[test]
    fn tombstoned_migrated_source_vertex_is_not_writable() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source_id = store.insert_vertex().expect("insert");
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

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");
        import_migrated_vertex(&store, bundle).expect("import");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");
        tombstone_migrated_vertex(&store, source_id).expect("tombstone");

        assert!(matches!(
            store.assert_local_vertex_writable(source_id),
            Err(GraphStoreError::VertexTombstoned)
        ));
    }
}
