//! PocketIC smoke test: router + index shard registry and active placement.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::{
    CommitVertexPlacementArgs, GlobalVertexId, ReleaseVertexPlacementArgs, VertexPlacement,
};
use gleaph_pocket_ic_tests::{SOURCE_SHARD, install_router_and_index, query_as_router};

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
fn router_registers_shards_and_commits_active_placement() {
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

    let _: () = update_as_graph(
        &env.pic,
        env.router,
        graph_source,
        "commit_vertex_placement",
        CommitVertexPlacementArgs {
            local_vertex_id: 42,
        },
    );

    let vertex_id = GlobalVertexId::new(SOURCE_SHARD, 42);
    let active = query_as_router(&env, env.router, "resolve_placement", vertex_id);
    assert!(matches!(
        active,
        VertexPlacement::Active(loc) if loc.shard_id == SOURCE_SHARD && loc.local_vertex_id == 42
    ));

    let _: () = update_as_graph(
        &env.pic,
        env.router,
        graph_source,
        "release_vertex_placement",
        ReleaseVertexPlacementArgs {
            local_vertex_id: 42,
        },
    );
}
