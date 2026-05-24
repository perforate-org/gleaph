//! Multi-canister incremental vertex migration over PocketIC.

use gleaph_graph_kernel::federation::{
    BeginVertexMigrationArgs, FederatedExpandArgs, FederatedExpandDirection,
    FederatedExpandNeighbor, MigrationStagingArgs, MigrationStartResult, VertexPlacement,
};
use gleaph_pocket_ic_tests::{
    DEST_SHARD, SOURCE_SHARD, e2e_insert_edge, e2e_insert_vertex, install_federation,
    migration_status, query_as_router, resolve_placement, run_migration_until_ready,
    update_as_router,
};

/// Full graph-shard migration over PocketIC (copy, cutover, stub prune, federated expand).
#[test]
fn incremental_migration_copy_cutover_and_prune() {
    let env = install_federation();

    let migrant = e2e_insert_vertex(&env, env.graph_source);
    let neighbor = e2e_insert_vertex(&env, env.graph_source);
    // Canonical edge on neighbor.o (Y -> X); must survive source stub prune.
    e2e_insert_edge(
        &env,
        env.graph_source,
        neighbor.local_vertex_id,
        migrant.local_vertex_id,
    );

    let start: MigrationStartResult = update_as_router(
        &env,
        env.graph_source,
        "migration_start",
        BeginVertexMigrationArgs {
            logical_vertex_id: migrant.logical_vertex_id,
            destination_shard_id: DEST_SHARD,
        },
    );

    match resolve_placement(&env, migrant.logical_vertex_id) {
        VertexPlacement::Migrating { .. } => {}
        other => panic!("router placement after migration_start: {other:?}"),
    }

    let staging: MigrationStartResult = update_as_router(
        &env,
        env.graph_dest,
        "migration_staging_begin",
        MigrationStagingArgs {
            logical_vertex_id: migrant.logical_vertex_id,
            epoch: start.epoch,
            source_shard_id: SOURCE_SHARD,
            source_local_vertex_id: start.local_vertex_id,
            metadata_snapshot: start.metadata_snapshot,
        },
    );
    // First vertex on a fresh shard may use local_vertex_id 0; verify staging row exists.
    let dest_status = migration_status(&env, env.graph_dest, migrant.logical_vertex_id);
    assert!(
        dest_status.item.is_some(),
        "destination shard should track migration item after staging_begin"
    );

    run_migration_until_ready(&env, migrant.logical_vertex_id);

    let _: () = update_as_router(
        &env,
        env.graph_dest,
        "migration_cutover",
        migrant.logical_vertex_id,
    );
    let _: () = update_as_router(
        &env,
        env.graph_source,
        "migration_cutover",
        migrant.logical_vertex_id,
    );

    match resolve_placement(&env, migrant.logical_vertex_id) {
        VertexPlacement::Active(loc) => {
            assert_eq!(loc.shard_id, DEST_SHARD);
            assert_eq!(loc.local_vertex_id, staging.local_vertex_id);
        }
        other => panic!("expected Active placement after cutover, got {other:?}"),
    }

    // Drain stub prune queue via maintenance ticks on the source shard.
    for _ in 0..64 {
        let _: Option<gleaph_graph_kernel::federation::MigrationApplyChunk> =
            update_as_router(&env, env.graph_source, "migration_maintenance_tick", ());
        let status = migration_status(&env, env.graph_source, migrant.logical_vertex_id);
        if status.item.is_none() {
            break;
        }
    }

    let status = migration_status(&env, env.graph_source, migrant.logical_vertex_id);
    assert!(
        status.item.is_none(),
        "migration queue should be empty after cutover cleanup"
    );

    // Neighbor still has its outgoing edge to the migrant logical id.
    let neighbors: Vec<FederatedExpandNeighbor> = query_as_router(
        &env,
        env.graph_source,
        "federated_expand",
        FederatedExpandArgs {
            logical_vertex_id: migrant.logical_vertex_id,
            direction: FederatedExpandDirection::Incoming,
            label_id_raw: None,
        },
    );
    assert!(
        neighbors
            .iter()
            .any(|n| n.neighbor_logical_vertex_id == neighbor.logical_vertex_id),
        "incoming expand via forwarding stub should reach neighbor"
    );
}
