//! PocketIC: router calls graph `finalize_bulk_ingest` after bulk edge ingest.

use gleaph_graph_kernel::federation::{BulkIngestFinalizeArgs, BulkIngestFinalizeResult};
use gleaph_pocket_ic_tests::{
    SOURCE_SHARD, e2e_insert_edge, e2e_insert_vertex, install_federation, update_as_router,
};

#[test]
fn graph_finalize_bulk_ingest_enqueues_forward_vertex_as_router() {
    let env = install_federation();
    let src = e2e_insert_vertex(&env, env.graph_source);
    let dst = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_edge(
        &env,
        env.graph_source,
        src.local_vertex_id,
        dst.local_vertex_id,
    );

    let result: BulkIngestFinalizeResult = update_as_router(
        &env,
        env.graph_source,
        "finalize_bulk_ingest",
        BulkIngestFinalizeArgs {
            target_shard_id: SOURCE_SHARD,
            forward_vertices: vec![src.local_vertex_id],
            reverse_vertices: vec![],
            enqueue: true,
        },
    );

    assert_eq!(result.queued_forward, 1);
    assert_eq!(result.queued_reverse, 0);
}

#[test]
fn graph_finalize_bulk_ingest_drain_only_retry_as_router() {
    let env = install_federation();

    let result: BulkIngestFinalizeResult = update_as_router(
        &env,
        env.graph_source,
        "finalize_bulk_ingest",
        BulkIngestFinalizeArgs {
            target_shard_id: SOURCE_SHARD,
            forward_vertices: vec![],
            reverse_vertices: vec![],
            enqueue: false,
        },
    );

    assert_eq!(result.queued_forward, 0);
    assert_eq!(result.queued_reverse, 0);
    assert_eq!(result.remaining_queue_len, 0);
}
