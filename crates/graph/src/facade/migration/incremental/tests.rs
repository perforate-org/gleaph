use super::*;
    use crate::facade::migration::prune_migrated_source_maintenance_step_for;
    use crate::facade::migration::vertex::{export_out_edge, import_out_edge};
    use crate::facade::mutation_executor::GraphMutationExecutor;
    use crate::facade::store::helpers::lara_label;
    use crate::facade::{FederationRouting, GraphStore};
    use crate::index::placement;
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::EdgeTarget;
    use gleaph_graph_kernel::federation::{
        BeginVertexMigrationArgs, FederatedExpandArgs, FederatedExpandDirection, LocalVertexId,
        MigrationStagingArgs, ShardId, VertexPlacement,
    };
    use ic_stable_lara::traits::{CsrEdge, CsrEdgeTombstone};

    const E2E_SOURCE_SHARD: ShardId = 7;
    const E2E_DEST_SHARD: ShardId = 9;
    const E2E_ROUTER: Principal = Principal::management_canister();

    fn e2e_routing(shard_id: ShardId) -> FederationRouting {
        FederationRouting {
            router_canister: E2E_ROUTER,
            index_canister: E2E_ROUTER,
            shard_id,
        }
    }

    fn e2e_set_shard(store: &GraphStore, shard_id: ShardId) {
        store
            .set_federation_routing(Some(e2e_routing(shard_id)))
            .expect("set federation routing");
    }

    /// Drive source maintenance and apply chunks on the destination until cutover-ready.
    fn run_migration_copy_until_ready(
        store: &GraphStore,
        logical: LogicalVertexId,
    ) -> (LocalVertexId, LocalVertexId) {
        const MAX_STEPS: usize = 512;
        for step in 0..MAX_STEPS {
            let status = migration_status(store, logical).expect("migration status");
            if status.item.is_none() {
                panic!("migration item missing at step {step}");
            }
            if status.ready_for_cutover {
                let item = status.item.expect("migration item when ready");
                return (item.source_local_vertex_id, item.target_local_vertex_id);
            }
            e2e_set_shard(store, E2E_SOURCE_SHARD);
            if let Some(chunk) = pollster::block_on(migration_maintenance_step_for(store, logical))
                .expect("maintenance step")
            {
                e2e_set_shard(store, E2E_DEST_SHARD);
                pollster::block_on(migration_apply_chunk(store, chunk)).expect("apply chunk");
            }
            if step + 1 == MAX_STEPS {
                panic!(
                    "migration not ready after {MAX_STEPS} steps; last phase {:?}",
                    status.item.map(|i| i.phase)
                );
            }
        }
        unreachable!()
    }

    #[test]
    fn source_migrating_visible_staging_hidden() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let logical = store.logical_vertex_id(source).expect("logical");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: 9,
            },
        ))
        .expect("start");

        assert!(vertex_visible_to_query(source));
        assert!(matches!(
            vertex_migration_state(source),
            VertexMigrationState::SourceMigrating { .. }
        ));

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: 7,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        let staging_id = VertexId::from(staging.local_vertex_id);
        assert!(!vertex_visible_to_query(staging_id));
    }

    fn install_w2_weight_profile(
        store: &GraphStore,
        label_id: gleaph_graph_kernel::entry::EdgeLabelId,
    ) {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
    }

    #[test]
    fn journal_out_edge_value_changed_applies_on_staging() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("MigrationJournalValue")
            .expect("label");
        install_w2_weight_profile(&store, label_id);

        pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: 9,
            },
        ))
        .expect("start");

        let source_handle = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[1, 0])
            .expect("edge");
        let source_wire =
            migration_wire_handle(source, source_handle.label_id, source_handle.slot_index);

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let epoch = pollster::block_on(resolve_migrating_epoch(&store, logical)).expect("epoch");
        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch,
                source_shard_id: 7,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: Default::default(),
            },
        ))
        .expect("staging");
        let staging_id = VertexId::from(staging.local_vertex_id);

        let exported = export_out_edge(
            &store,
            source,
            &store
                .find_outgoing_edge_record(source_handle)
                .expect("lookup")
                .expect("edge"),
        )
        .expect("export");
        import_out_edge(&store, staging_id, &exported).expect("import staging edge");
        MIGRATION_OUT_HANDLE_MAP.with_borrow_mut(|m| {
            m.insert(
                logical,
                epoch,
                source_wire,
                migration_wire_handle(staging_id, source_handle.label_id, source_handle.slot_index),
            );
        });

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        store
            .update_edge_value_at_handle(source_handle, &[9, 0])
            .expect("source value change");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let item = load_item(logical).expect("migration item");
        let entry = MigrationJournalEntry {
            logical_vertex_id: logical,
            epoch,
            seq: 99,
            op: MigrationJournalOp::OutEdgeValueChanged {
                source_handle: source_wire,
                value_bytes: vec![9, 0],
            },
        };
        pollster::block_on(apply_journal_to_staging(&store, staging_id, &item, &entry))
            .expect("apply value journal");

        let staging_handle =
            EdgeHandle::at_slot(staging_id, source_handle.label_id, source_handle.slot_index);
        let edge = store
            .find_outgoing_edge_record(staging_handle)
            .expect("staging lookup")
            .expect("staging edge");
        assert_eq!(edge.value_bytes(), &[9, 0]);
    }

    #[test]
    fn journal_out_edge_removed_applies_on_staging() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("MigrationJournalRemove")
            .expect("label");
        install_w2_weight_profile(&store, label_id);

        pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: 9,
            },
        ))
        .expect("start");

        let source_handle = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[1, 0])
            .expect("edge");
        let source_wire =
            migration_wire_handle(source, source_handle.label_id, source_handle.slot_index);
        let epoch = pollster::block_on(resolve_migrating_epoch(&store, logical)).expect("epoch");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch,
                source_shard_id: 7,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: Default::default(),
            },
        ))
        .expect("staging");
        let staging_id = VertexId::from(staging.local_vertex_id);

        let exported = export_out_edge(
            &store,
            source,
            &store
                .find_outgoing_edge_record(source_handle)
                .expect("lookup")
                .expect("edge"),
        )
        .expect("export");
        import_out_edge(&store, staging_id, &exported).expect("import staging edge");
        let staging_wire =
            migration_wire_handle(staging_id, source_handle.label_id, source_handle.slot_index);
        MIGRATION_OUT_HANDLE_MAP.with_borrow_mut(|m| {
            m.insert(logical, epoch, source_wire, staging_wire);
        });

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("source routing");

        store
            .delete_edge_by_handle(source_handle)
            .expect("delete source edge");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let item = load_item(logical).expect("migration item");
        let entry = MigrationJournalEntry {
            logical_vertex_id: logical,
            epoch,
            seq: 100,
            op: MigrationJournalOp::OutEdgeRemoved {
                source_handle: source_wire,
            },
        };
        pollster::block_on(apply_journal_to_staging(&store, staging_id, &item, &entry))
            .expect("apply remove journal");

        let staging_handle =
            EdgeHandle::at_slot(staging_id, source_handle.label_id, source_handle.slot_index);
        assert!(
            store
                .find_outgoing_edge_record(staging_handle)
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn journal_remote_out_edge_added_applies_on_staging() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let logical = store.logical_vertex_id(source).expect("logical");
        let remote_logical = 44_001;

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");
        let staging_id = VertexId::from(staging.local_vertex_id);

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        store
            .insert_directed_edge_to_logical(source, remote_logical, None)
            .expect("remote edge");

        let entries = MIGRATION_JOURNAL.with_borrow(|j| j.entries_for(logical, start.epoch, 0, 0));
        assert_eq!(entries.len(), 1);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let item = load_item(logical).expect("migration item");
        pollster::block_on(apply_journal_to_staging(
            &store,
            staging_id,
            &item,
            &entries[0],
        ))
        .expect("apply remote add journal");

        let copied = store
            .directed_out_edges(staging_id)
            .expect("staging out")
            .into_iter()
            .filter(|edge| {
                matches!(
                    edge.edge_target(),
                    Some(EdgeTarget::Remote(remote_ref))
                        if store.logical_vertex_for_remote_ref(remote_ref) == Some(remote_logical)
                )
            })
            .count();
        assert_eq!(copied, 1);
    }

    #[test]
    fn journal_remote_parallel_out_edge_maps_value_updates_to_inserted_edge() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let logical = store.logical_vertex_id(source).expect("logical");
        let remote_logical = 44_002;
        let label_id = store
            .get_or_insert_edge_label_id("RemoteParallelJournal")
            .expect("label");
        install_w2_weight_profile(&store, label_id);

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");
        let staging_id = VertexId::from(staging.local_vertex_id);

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        let first = store
            .insert_directed_edge_to_logical_with_value_bytes(
                source,
                remote_logical,
                Some(label_id),
                &[1, 0],
            )
            .expect("first remote edge");
        let second = store
            .insert_directed_edge_to_logical_with_value_bytes(
                source,
                remote_logical,
                Some(label_id),
                &[2, 0],
            )
            .expect("second remote edge");

        let entries = MIGRATION_JOURNAL.with_borrow(|j| j.entries_for(logical, start.epoch, 0, 1));
        assert_eq!(entries.len(), 2);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let item = load_item(logical).expect("migration item");
        for entry in &entries {
            pollster::block_on(apply_journal_to_staging(&store, staging_id, &item, entry))
                .expect("apply remote add journal");
        }

        let first_wire = migration_wire_handle(source, first.label_id, first.slot_index);
        let second_wire = migration_wire_handle(source, second.label_id, second.slot_index);
        assert!(
            MIGRATION_OUT_HANDLE_MAP
                .with_borrow(|m| m.get(logical, start.epoch, first_wire))
                .is_some()
        );
        assert!(
            MIGRATION_OUT_HANDLE_MAP
                .with_borrow(|m| m.get(logical, start.epoch, second_wire))
                .is_some()
        );

        let update = MigrationJournalEntry {
            logical_vertex_id: logical,
            epoch: start.epoch,
            seq: 2,
            op: MigrationJournalOp::OutEdgeValueChanged {
                source_handle: second_wire,
                value_bytes: vec![9, 0],
            },
        };
        pollster::block_on(apply_journal_to_staging(&store, staging_id, &item, &update))
            .expect("apply value update");

        let values = store
            .directed_out_edges(staging_id)
            .expect("staging out")
            .into_iter()
            .filter(|edge| {
                matches!(
                    edge.edge_target(),
                    Some(EdgeTarget::Remote(remote_ref))
                        if store.logical_vertex_for_remote_ref(remote_ref) == Some(remote_logical)
                )
            })
            .map(|edge| edge.value_bytes().to_vec())
            .collect::<Vec<_>>();
        assert!(values.contains(&vec![1, 0]));
        assert!(values.contains(&vec![9, 0]));
        assert!(!values.contains(&vec![2, 0]));
    }

    #[test]
    fn journal_in_reverse_added_marks_non_self_predecessor_remote() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let logical = store.logical_vertex_id(target).expect("target logical");
        let source_logical = store.logical_vertex_id(source).expect("source logical");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        store
            .insert_directed_edge(source, target, None)
            .expect("incoming edge");

        let entries = MIGRATION_JOURNAL.with_borrow(|j| j.entries_for(logical, start.epoch, 0, 0));
        assert_eq!(entries.len(), 1);
        let MigrationJournalOp::InReverseAdded {
            predecessor_logical_vertex_id,
            predecessor_is_remote,
            ..
        } = &entries[0].op
        else {
            panic!("expected InReverseAdded");
        };
        assert_eq!(*predecessor_logical_vertex_id, source_logical);
        assert!(*predecessor_is_remote);
    }

    #[test]
    fn journal_undirected_edge_added_applies_for_migrating_alias_endpoint() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let neighbor = store.insert_vertex().expect("neighbor");
        let source = store.insert_vertex().expect("source");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("JournalUndirectedAdd")
            .expect("label");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");
        let staging_id = VertexId::from(staging.local_vertex_id);

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        store
            .insert_undirected_edge(source, neighbor, Some(label_id))
            .expect("undirected edge");

        let entries = MIGRATION_JOURNAL.with_borrow(|j| j.entries_for(logical, start.epoch, 0, 0));
        assert_eq!(entries.len(), 1);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let item = load_item(logical).expect("migration item");
        pollster::block_on(apply_journal_to_staging(
            &store,
            staging_id,
            &item,
            &entries[0],
        ))
        .expect("apply undirected add journal");

        let copied = store
            .undirected_edges(staging_id)
            .expect("staging undirected")
            .into_iter()
            .filter(|edge| !edge.is_tombstone_edge())
            .count();
        assert_eq!(copied, 1);
    }

    #[test]
    fn native_pending_apply_delivers_maintenance_chunk() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let neighbor = store.insert_vertex().expect("neighbor");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("PendingApply")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        store
            .insert_directed_edge_with_value_bytes(source, neighbor, Some(label_id), &[1, 0])
            .expect("edge");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        let chunk = pollster::block_on(migration_maintenance_step_for(&store, logical))
            .expect("maintenance")
            .expect("first chunk");
        set_native_pending_apply(chunk.clone());
        let pending = take_native_pending_apply().expect("pending chunk");
        assert_eq!(pending.logical_vertex_id, logical);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_apply_chunk(&store, pending)).expect("apply pending chunk");
        let item = load_item(logical).expect("item");
        assert_ne!(item.phase, MigrationPhase::VertexMetadata);
    }

    #[test]
    fn journal_out_edge_property_set_applies_on_staging() {
        use gleaph_graph_kernel::entry::PropertyId;

        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("MigrationJournalProperty")
            .expect("label");
        install_w2_weight_profile(&store, label_id);

        pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: 9,
            },
        ))
        .expect("start");

        let source_handle = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[1, 0])
            .expect("edge");
        let source_wire =
            migration_wire_handle(source, source_handle.label_id, source_handle.slot_index);
        let epoch = pollster::block_on(resolve_migrating_epoch(&store, logical)).expect("epoch");

        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 9,
            }))
            .expect("dest routing");

        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch,
                source_shard_id: 7,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: Default::default(),
            },
        ))
        .expect("staging");
        let staging_id = VertexId::from(staging.local_vertex_id);

        let exported = export_out_edge(
            &store,
            source,
            &store
                .find_outgoing_edge_record(source_handle)
                .expect("lookup")
                .expect("edge"),
        )
        .expect("export");
        import_out_edge(&store, staging_id, &exported).expect("import");
        MIGRATION_OUT_HANDLE_MAP.with_borrow_mut(|m| {
            m.insert(
                logical,
                epoch,
                source_wire,
                migration_wire_handle(staging_id, source_handle.label_id, source_handle.slot_index),
            );
        });

        let prop = PropertyId::from_raw(42);
        let value_bytes = Value::Int64(88).to_binary_bytes().expect("encode");
        let item = load_item(logical).expect("item");
        let entry = MigrationJournalEntry {
            logical_vertex_id: logical,
            epoch,
            seq: 7,
            op: MigrationJournalOp::OutEdgePropertySet {
                source_handle: source_wire,
                property_id: prop,
                value_bytes,
            },
        };
        pollster::block_on(apply_journal_to_staging(&store, staging_id, &item, &entry))
            .expect("apply property journal");

        let staging_handle =
            EdgeHandle::at_slot(staging_id, source_handle.label_id, source_handle.slot_index);
        assert_eq!(
            store.edge_property(staging_handle, prop),
            Some(Value::Int64(88))
        );
    }

    #[cfg(test)]
    fn clear_migration_queue_for_test() {
        let stale = MIGRATION_QUEUE.with_borrow(|q| q.logical_ids());
        for logical in stale {
            remove_item(logical);
        }
        crate::facade::migration::prune_source::clear_prune_queue_for_test();
    }

    fn run_prune_until_done(store: &GraphStore, logical: LogicalVertexId) {
        for _ in 0..256 {
            if !prune_queue_has_item(logical) {
                return;
            }
            prune_migrated_source_maintenance_step_for(store, logical).expect("prune step");
        }
        panic!("prune queue did not drain for logical {logical}");
    }

    #[test]
    fn migration_reconcile_rebuilds_lost_queue_item() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let neighbor = store.insert_vertex().expect("neighbor");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("ReconcileRebuild")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        store
            .insert_directed_edge_with_value_bytes(source, neighbor, Some(label_id), &[1, 0])
            .expect("edge");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        let chunk = pollster::block_on(migration_maintenance_step_for(&store, logical))
            .expect("step")
            .expect("chunk");
        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_apply_chunk(&store, chunk)).expect("apply");

        remove_item(logical);
        assert!(load_item(logical).is_none());

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        let report = pollster::block_on(migration_reconcile(&store, logical)).expect("reconcile");
        assert_eq!(report.action, MigrationReconcileAction::RebuiltQueueItem);
        let item = load_item(logical).expect("rebuilt item");
        assert_eq!(item.epoch, start.epoch);
        assert_ne!(item.target_local_vertex_id, 0);
    }

    #[test]
    fn migration_reconcile_clears_stale_epoch_item() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        let source = store.insert_vertex().expect("source");
        let logical = store.logical_vertex_id(source).expect("logical");

        pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        let mut item = load_item(logical).expect("item");
        item.epoch = item.epoch.saturating_add(999);
        save_item(item);

        let report = pollster::block_on(migration_reconcile(&store, logical)).expect("reconcile");
        assert!(matches!(
            report.action,
            MigrationReconcileAction::RemovedStaleEpoch { .. }
        ));
        assert!(load_item(logical).is_none());
    }

    /// Full native path: start → staging → maintenance chunks → apply → cutover.
    /// Live journal value/property replay is covered by `journal_out_edge_*` tests.
    #[test]
    fn incremental_migration_e2e_copy_and_cutover() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store
            .insert_vertex_named(["Migrant"], [("score", Value::Int64(10))])
            .expect("source");
        let neighbor = store.insert_vertex().expect("neighbor");
        let logical = store.logical_vertex_id(source).expect("logical");
        let neighbor_logical = store.logical_vertex_id(neighbor).expect("neighbor logical");

        let label_id = store
            .get_or_insert_edge_label_id("E2E_MIGRATE")
            .expect("label");
        install_w2_weight_profile(&store, label_id);

        let edge_value = [5u8, 0];
        store
            .insert_directed_edge_with_value_bytes(source, neighbor, Some(label_id), &edge_value)
            .expect("out edge");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("migration_start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot.clone(),
            },
        ))
        .expect("migration_staging_begin");
        let staging_id = VertexId::from(staging.local_vertex_id);
        assert!(!vertex_visible_to_query(staging_id));

        let (source_local, target_local) = run_migration_copy_until_ready(&store, logical);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("dest cutover");
        // cutover + assertions below must not call neighbor_vid() on remote out-edges
        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("source cutover");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        match pollster::block_on(placement::resolve_placement(E2E_ROUTER, logical))
            .expect("placement")
        {
            VertexPlacement::Active(loc) => {
                assert_eq!(loc.shard_id, E2E_DEST_SHARD);
                assert_eq!(loc.local_vertex_id, target_local);
            }
            other => panic!("expected Active on dest after cutover, got {other:?}"),
        }

        let dest_id = VertexId::from(target_local);
        assert!(vertex_visible_to_query(dest_id));
        assert!(matches!(
            vertex_migration_state(dest_id),
            VertexMigrationState::Active
        ));

        let dest_vertex = store.vertex(dest_id).expect("dest vertex");
        let labels = store.vertex_labels(dest_id, dest_vertex);
        assert!(
            labels
                .iter()
                .any(|l| store.vertex_label_name(*l).as_deref() == Some("Migrant")),
            "metadata labels copied to destination"
        );
        let score_id = store.get_or_insert_property_id("score").expect("prop");
        assert_eq!(
            store.vertex_property(dest_id, score_id),
            Some(Value::Int64(10))
        );

        use crate::facade::store::helpers::{edge_storage_label, lara_label};
        let wire_label = lara_label(edge_storage_label(Some(label_id), false));
        let remote_edge = store
            .find_first_forward_handle_descending(dest_id, wire_label, |edge| {
                matches!(edge.edge_target(), Some(EdgeTarget::Remote(_)))
            })
            .expect("lookup")
            .expect("remote out-edge on destination");
        let edge = store
            .find_outgoing_edge_record(remote_edge)
            .expect("edge record")
            .expect("edge payload");
        assert_eq!(edge.value_bytes(), &edge_value);

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        let source_id = VertexId::from(source_local);
        assert!(!vertex_visible_to_query(source_id));
        assert!(matches!(
            vertex_migration_state(source_id),
            VertexMigrationState::ForwardingStub { .. }
        ));
        assert!(
            prune_queue_has_item(logical),
            "source cutover should enqueue stub cleanup"
        );
        run_prune_until_done(&store, logical);
        assert!(!stub_has_live_edge_payload(&store, source_id));
        assert!(!stub_has_vertex_payload(&store, source_id));
        assert!(matches!(
            vertex_migration_state(source_id),
            VertexMigrationState::ForwardingStub { .. }
        ));

        let _ = neighbor_logical;
    }

    #[test]
    fn active_migration_state_does_not_keep_visibility_filter_enabled() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        let vertex = store.insert_vertex().expect("vertex");

        assert!(!migration_visibility_filter_needed());
        set_migration_state(vertex, VertexMigrationState::TargetStaging { epoch: 1 });
        assert!(migration_visibility_filter_needed());

        set_migration_state(vertex, VertexMigrationState::Active);
        assert!(matches!(
            vertex_migration_state(vertex),
            VertexMigrationState::Active
        ));
        assert!(
            !migration_visibility_filter_needed(),
            "Active is the default state and must not leave a stored map entry behind"
        );
    }

    #[test]
    fn prune_clears_stub_payload_preserves_neighbor_edges() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let neighbor = store.insert_vertex().expect("neighbor");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("PRUNE_STUB")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        // Neighbor -> migrant: canonical edge lives on neighbor.o and must survive source prune.
        let incoming_handle = store
            .insert_directed_edge_with_value_bytes(neighbor, source, Some(label_id), &[3, 0])
            .expect("edge into migrant");
        let property_id = store.get_or_insert_property_id("kept").expect("property");
        store
            .set_edge_property(incoming_handle, property_id, Value::Text("yes".into()))
            .expect("edge property");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        let (source_local, _) = run_migration_copy_until_ready(&store, logical);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("dest cutover");
        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("source cutover");

        let source_id = VertexId::from(source_local);
        assert!(stub_has_live_edge_payload(&store, source_id));
        run_prune_until_done(&store, logical);
        assert!(!stub_has_live_edge_payload(&store, source_id));
        assert!(matches!(
            vertex_migration_state(source_id),
            VertexMigrationState::ForwardingStub { .. }
        ));

        let neighbor_out = store
            .directed_out_edges(neighbor)
            .expect("neighbor out")
            .into_iter()
            .filter(|e| !e.is_tombstone_edge())
            .count();
        assert_eq!(
            neighbor_out, 1,
            "neighbor canonical out-edge to migrant must not be removed by source-local prune"
        );
        assert_eq!(
            store.edge_property(incoming_handle, property_id),
            Some(Value::Text("yes".into())),
            "source-local reverse prune must not clear neighbor-owned canonical edge properties"
        );
    }

    #[test]
    fn prune_clears_stub_undirected_payload_preserves_neighbor_row() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let neighbor = store.insert_vertex().expect("neighbor");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("PRUNE_STUB_UNDIRECTED")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        store
            .insert_undirected_edge_with_value_bytes(source, neighbor, Some(label_id), &[4, 0])
            .expect("undirected edge");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        let (source_local, _) = run_migration_copy_until_ready(&store, logical);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("dest cutover");
        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("source cutover");

        let source_id = VertexId::from(source_local);
        assert!(stub_has_live_edge_payload(&store, source_id));
        run_prune_until_done(&store, logical);
        assert!(!stub_has_live_edge_payload(&store, source_id));

        let neighbor_undirected = store
            .undirected_edges(neighbor)
            .expect("neighbor undirected")
            .into_iter()
            .filter(|e| !e.is_tombstone_edge())
            .count();
        assert_eq!(
            neighbor_undirected, 1,
            "source-local prune must not remove the neighbor's undirected row"
        );
    }

    #[test]
    fn migration_copies_undirected_edges_before_source_prune() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let neighbor = store.insert_vertex().expect("neighbor");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("MIGRATE_UNDIRECTED")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        store
            .insert_undirected_edge_with_value_bytes(source, neighbor, Some(label_id), &[6, 0])
            .expect("undirected edge");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        let (_, target_local) = run_migration_copy_until_ready(&store, logical);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("dest cutover");
        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("source cutover");
        run_prune_until_done(&store, logical);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let dest_undirected = store
            .undirected_edges(VertexId::from(target_local))
            .expect("dest undirected")
            .into_iter()
            .filter(|e| !e.is_tombstone_edge())
            .count();
        assert_eq!(
            dest_undirected, 1,
            "destination keeps the migrated undirected edge after source cleanup"
        );
    }

    #[test]
    fn incoming_expand_uses_local_forward_scan_after_stub_reverse_prune() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let neighbor = store.insert_vertex().expect("neighbor");
        let logical = store.logical_vertex_id(source).expect("logical");
        let neighbor_logical = store.logical_vertex_id(neighbor).expect("neighbor logical");
        store
            .insert_directed_edge(neighbor, source, None)
            .expect("incoming edge");

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        run_migration_copy_until_ready(&store, logical);

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("dest cutover");
        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        pollster::block_on(migration_cutover(&store, logical)).expect("source cutover");
        run_prune_until_done(&store, logical);

        let hits = pollster::block_on(crate::facade::federation_expand::collect_federated_expand(
            &store,
            FederatedExpandArgs {
                logical_vertex_id: logical,
                direction: FederatedExpandDirection::Incoming,
                label_id_raw: None,
            },
        ))
        .expect("incoming expand");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].neighbor_logical_vertex_id, neighbor_logical);
    }

    #[test]
    fn in_reverse_journal_maps_to_the_inserted_destination_row() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_DEST_SHARD);

        let target = store.insert_vertex().expect("target");
        let logical = store.logical_vertex_id(target).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("ReverseJournalHandle")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        let lara_label = LaraLabelId::from_raw(
            label_id
                .pack(gleaph_graph_kernel::entry::EdgeDirectedness::Directed)
                .raw(),
        );
        let epoch = 77;
        let source_a = migration_wire_handle(VertexId::from(10), lara_label, 1);
        let source_b = migration_wire_handle(VertexId::from(10), lara_label, 2);

        import_in_reverse_edge(
            &store,
            target,
            logical,
            epoch,
            &ExportedInReverseEdge {
                catalog_label: Some(label_id),
                value_bytes: vec![1, 0],
                predecessor_logical_vertex_id: 90_001,
                predecessor_is_remote: true,
                source_reverse_handle: source_a,
                canonical_source_handle: source_a,
                properties: vec![],
            },
        )
        .expect("import first reverse");
        import_in_reverse_edge(
            &store,
            target,
            logical,
            epoch,
            &ExportedInReverseEdge {
                catalog_label: Some(label_id),
                value_bytes: vec![2, 0],
                predecessor_logical_vertex_id: 90_002,
                predecessor_is_remote: true,
                source_reverse_handle: source_b,
                canonical_source_handle: source_b,
                properties: vec![],
            },
        )
        .expect("import second reverse");

        let mut item = MigrationItem::new(
            logical,
            epoch,
            E2E_SOURCE_SHARD,
            placement::local_vertex_id_raw(VertexId::from(10)),
            E2E_DEST_SHARD,
        );
        item.target_local_vertex_id = placement::local_vertex_id_raw(target);
        let entry = MigrationJournalEntry {
            logical_vertex_id: logical,
            epoch,
            seq: 0,
            op: MigrationJournalOp::InReverseValueChanged {
                source_handle: source_b,
                value_bytes: vec![9, 0],
            },
        };
        pollster::block_on(apply_journal_to_staging(&store, target, &item, &entry))
            .expect("apply reverse value journal");

        let target_a = MIGRATION_REV_HANDLE_MAP
            .with_borrow(|m| m.get(logical, epoch, source_a))
            .expect("first target handle");
        let target_b = MIGRATION_REV_HANDLE_MAP
            .with_borrow(|m| m.get(logical, epoch, source_b))
            .expect("second target handle");
        assert_ne!(target_a.slot_index, target_b.slot_index);

        let first = store
            .find_outgoing_edge_record(handle_from_wire(target, target_a))
            .expect("first lookup")
            .expect("first edge");
        let second = store
            .find_outgoing_edge_record(handle_from_wire(target, target_b))
            .expect("second lookup")
            .expect("second edge");
        assert_eq!(first.value_bytes(), &[1, 0]);
        assert_eq!(second.value_bytes(), &[9, 0]);
    }

    #[test]
    fn canonical_incoming_edge_mutations_journal_to_migrating_target_reverse_row() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let pred = store.insert_vertex().expect("pred");
        let source = store.insert_vertex().expect("source");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("IncomingCanonicalJournal")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        let incoming = store
            .insert_directed_edge_with_value_bytes(pred, source, Some(label_id), &[1, 0])
            .expect("incoming edge");
        let wire_label =
            lara_label(label_id.pack(gleaph_graph_kernel::entry::EdgeDirectedness::Directed));
        let source_reverse = store
            .find_first_reverse_handle_descending(source, wire_label, |edge| {
                edge.neighbor_vid() == pred
            })
            .expect("reverse lookup")
            .expect("source reverse row");
        let source_wire =
            migration_wire_handle(source, source_reverse.label_id, source_reverse.slot_index);

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        let (_, target_local) = run_migration_copy_until_ready(&store, logical);
        let target_id = VertexId::from(target_local);
        let target_wire = MIGRATION_REV_HANDLE_MAP
            .with_borrow(|m| m.get(logical, start.epoch, source_wire))
            .expect("target reverse handle");
        let target_handle = handle_from_wire(target_id, target_wire);

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        store
            .update_edge_value_at_handle(incoming, &[9, 0])
            .expect("incoming value update");
        let entries = MIGRATION_JOURNAL.with_borrow(|j| j.entries_for(logical, start.epoch, 0, 0));
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            entries[0].op,
            MigrationJournalOp::InReverseValueChanged { .. }
        ));

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let item = load_item(logical).expect("migration item");
        pollster::block_on(apply_journal_to_staging(
            &store,
            target_id,
            &item,
            &entries[0],
        ))
        .expect("apply value journal");
        let updated = store
            .find_outgoing_edge_record(target_handle)
            .expect("target lookup")
            .expect("target reverse row");
        assert_eq!(updated.value_bytes(), &[9, 0]);

        e2e_set_shard(&store, E2E_SOURCE_SHARD);
        store
            .delete_edge_by_handle(incoming)
            .expect("incoming delete");
        let entries = MIGRATION_JOURNAL.with_borrow(|j| j.entries_for(logical, start.epoch, 1, 1));
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            entries[0].op,
            MigrationJournalOp::InReverseRemoved { .. }
        ));

        e2e_set_shard(&store, E2E_DEST_SHARD);
        pollster::block_on(apply_journal_to_staging(
            &store,
            target_id,
            &item,
            &entries[0],
        ))
        .expect("apply remove journal");
        assert!(
            store
                .find_outgoing_edge_record(target_handle)
                .expect("target lookup after remove")
                .is_none()
        );
    }

    #[test]
    fn in_reverse_copy_is_resumable_and_preserves_label() {
        clear_migration_queue_for_test();
        let store = GraphStore::new();
        e2e_set_shard(&store, E2E_SOURCE_SHARD);

        let source = store.insert_vertex().expect("source");
        let logical = store.logical_vertex_id(source).expect("logical");
        let label_id = store
            .get_or_insert_edge_label_id("MIGRATE_IN_LABEL")
            .expect("label");
        install_w2_weight_profile(&store, label_id);
        for i in 0..5 {
            let pred = store.insert_vertex().expect("pred");
            store
                .insert_directed_edge_with_value_bytes(pred, source, Some(label_id), &[i, 0])
                .expect("incoming edge");
        }

        let start = pollster::block_on(migration_start(
            &store,
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: E2E_DEST_SHARD,
            },
        ))
        .expect("start");

        e2e_set_shard(&store, E2E_DEST_SHARD);
        let staging = pollster::block_on(migration_staging_begin(
            &store,
            MigrationStagingArgs {
                logical_vertex_id: logical,
                epoch: start.epoch,
                source_shard_id: E2E_SOURCE_SHARD,
                source_local_vertex_id: placement::local_vertex_id_raw(source),
                metadata_snapshot: start.metadata_snapshot,
            },
        ))
        .expect("staging");

        let mut item = load_item(logical).expect("item");
        item.bulk_limit = 2;
        save_item(item);

        assert_eq!(
            store
                .directed_in_edges(source)
                .expect("source in")
                .into_iter()
                .filter(|e| !e.is_tombstone_edge())
                .count(),
            5,
            "source test fixture should have five incoming reverse rows"
        );

        let (_, target_local) = run_migration_copy_until_ready(&store, logical);
        assert_eq!(target_local, staging.local_vertex_id);

        let label_raw = label_id
            .pack(gleaph_graph_kernel::entry::EdgeDirectedness::Directed)
            .raw();
        let dest_in_edges = store
            .directed_in_edges(VertexId::from(target_local))
            .expect("dest in")
            .into_iter()
            .filter(|e| !e.is_tombstone_edge())
            .collect::<Vec<_>>();
        let copied = dest_in_edges
            .iter()
            .filter(|e| e.label_id == label_raw)
            .count();
        assert_eq!(
            copied,
            5,
            "reverse copy advances across multiple chunks and keeps the labeled bucket; all labels: {:?}",
            dest_in_edges.iter().map(|e| e.label_id).collect::<Vec<_>>()
        );
        let mut copied_values = dest_in_edges
            .iter()
            .filter(|e| e.label_id == label_raw)
            .map(|e| e.value_bytes().to_vec())
            .collect::<Vec<_>>();
        copied_values.sort();
        assert_eq!(
            copied_values,
            (0..5).map(|i| vec![i, 0]).collect::<Vec<_>>(),
            "reverse copy must preserve canonical forward values"
        );
    }
