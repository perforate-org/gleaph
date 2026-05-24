//! PocketIC smoke test: router + index shard registry and placement migration.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::{
    BeginVertexMigrationArgs, CommitVertexPlacementArgs, FinishVertexMigrationArgs, VertexPlacement,
};
use gleaph_pocket_ic_tests::{DEST_SHARD, install_router_and_index, query_as_router};

fn update_as_graph<
    T: candid::CandidType + serde::de::DeserializeOwned,
    R: candid::CandidType + serde::de::DeserializeOwned,
>(
    pic: &pocket_ic::PocketIc,
    router: Principal,
    graph: Principal,
    method: &str,
    args: T,
) -> R {
    let bytes = pic
        .update_call(router, graph, method, Encode!(&args).expect("encode"))
        .unwrap_or_else(|e| panic!("{method}: {e:?}"));
    match candid::Decode!(&bytes, Result<R, gleaph_graph_kernel::federation::RouterError>) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("{method} rejected: {err:?}"),
        Err(err) => panic!("decode {method}: {err}"),
    }
}

#[test]
fn router_registers_shards_and_runs_placement_migration() {
    let env = install_router_and_index();

    let graph_source = env.pic.create_canister();
    env.pic.add_cycles(graph_source, 2_000_000_000_000);
    let graph_dest = env.pic.create_canister();
    env.pic.add_cycles(graph_dest, 2_000_000_000_000);

    gleaph_pocket_ic_tests::register_graph_and_shards(
        &env.pic,
        env.admin,
        env.router,
        env.index,
        graph_source,
        graph_dest,
    );

    let logical = update_as_graph(
        &env.pic,
        env.router,
        graph_source,
        "allocate_logical_vertex_id",
        (),
    );
    let _: () = update_as_graph(
        &env.pic,
        env.router,
        graph_source,
        "commit_vertex_placement",
        CommitVertexPlacementArgs {
            logical_vertex_id: logical,
            local_vertex_id: 42,
        },
    );

    let _: () = update_as_graph(
        &env.pic,
        env.router,
        graph_source,
        "begin_vertex_migration",
        BeginVertexMigrationArgs {
            logical_vertex_id: logical,
            destination_shard_id: DEST_SHARD,
        },
    );

    let migrating = query_as_router(&env, env.router, "resolve_placement", logical);
    assert!(matches!(
        migrating,
        VertexPlacement::Migrating {
            destination_shard_id: DEST_SHARD,
            ..
        }
    ));

    let _: () = update_as_graph(
        &env.pic,
        env.router,
        graph_dest,
        "finish_vertex_migration",
        FinishVertexMigrationArgs {
            logical_vertex_id: logical,
            destination_local_vertex_id: 5,
        },
    );

    let active = query_as_router(&env, env.router, "resolve_placement", logical);
    assert!(matches!(
        active,
        VertexPlacement::Active(loc) if loc.shard_id == DEST_SHARD && loc.local_vertex_id == 5
    ));
}
