//! PocketIC red repros for ADR 0023 (federated index/store consistency across
//! the upgrade boundary).
//!
//! The shard-local index registry (`graph/src/index/registry.rs`) is a volatile
//! `thread_local` derived gate, and `#[post_upgrade]` does not rebuild it. As a
//! result a write that lands after a graph-shard upgrade emits no index posting
//! (the registry reports the property as not-indexed), while the router's index
//! catalog — which is stable — keeps planning index-served lookups. The index
//! and the primary store silently diverge: index-served queries miss the
//! post-upgrade write.
//!
//! This test goes through the real GQL mutation path (router → planner →
//! `ExecutePlanArgs` → shard), which is exactly the path ADR 0023 rewires to
//! carry a router-sourced catalog. It is RED today and becomes the GREEN target
//! once the registry is replaced by the ephemeral per-operation catalog.

use candid::{Decode, Encode};
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId};
use gleaph_graph_kernel::index::PostingHit;
use gleaph_graph_kernel::path::GraphPathVertexId;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_edge_label, admin_intern_property, create_edge_property_index,
    create_vertex_property_index, drain_maintenance_via_timer, drop_vertex_property_index,
    e2e_delete_directed_edge_with_property, e2e_enqueue_forward_compaction,
    e2e_insert_directed_edge_with_property, e2e_insert_vertex, e2e_maintenance_queue_len,
    e2e_reverse_resolved_edge_property, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_result_as_admin, gql_query_as_admin, install_single_shard_federation,
    wasm_bytes,
};

const INDEX_VERTEX_LABEL: &str = "Person";
const INDEX_AGE_NAME: &str = "adr0023_vertex_age";
const INDEX_EDGE_LABEL: &str = "KNOWS";
const INDEX_WEIGHT_NAME: &str = "adr0023_edge_weight";

/// Counts postings on graph-index whose value matches `age` (summed over the
/// small interned-property-id space the test uses). `lookup_equal` is
/// router-guarded and returns a bare `Vec<PostingHit>`.
fn count_postings_for_value(env: &FederationEnv, value: &[u8]) -> usize {
    let mut total = 0usize;
    for property_id in 0u32..16 {
        let bytes = env
            .pic
            .query_call(
                env.index,
                env.router,
                "lookup_equal",
                Encode!(&property_id, &value.to_vec()).expect("encode lookup_equal"),
            )
            .expect("lookup_equal query");
        let hits = Decode!(&bytes, Vec<PostingHit>).expect("decode lookup_equal");
        total += hits.len();
    }
    total
}

#[test]
fn post_upgrade_indexed_write_stays_consistent_with_store() {
    let env = install_single_shard_federation();
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "adr0023_create_index",
    );

    // Pre-upgrade indexed write: the posting is created and the index-served
    // equality lookup finds exactly the one matching vertex.
    let _ = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person {age: 5})",
        "adr0023_pre_upgrade_insert",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    let before = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(
        before.row_count, 1,
        "pre-upgrade indexed query must find the single matching vertex"
    );

    // Upgrade only the graph shard: its volatile registry is dropped and
    // post_upgrade does not rebuild it (ADR 0023 P1). The router keeps the index
    // in its (stable) catalog, so it still plans index-served lookups.
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");

    // Post-upgrade indexed write of a second vertex with the SAME indexed value.
    let _ = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person {age: 5})",
        "adr0023_post_upgrade_insert",
    );
    drain_maintenance_via_timer(&env, env.graph_source);

    // The index-served query must now observe BOTH vertices. Today it returns
    // only 1: the post-upgrade write emitted no posting (registry empty), so the
    // index diverged from the store. This is the red repro for P1.
    let after = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(
        after.row_count, 2,
        "post-upgrade indexed write must be visible through the index \
         (P1: shard registry volatility loses the posting)"
    );
}

/// ADR 0023 INV (P1, edge variant): an indexed **edge** write that lands after a
/// graph-shard upgrade must still emit its posting via the router-sourced
/// ephemeral catalog, so the edge index stays consistent with the store across
/// the upgrade boundary.
#[test]
fn post_upgrade_indexed_edge_write_stays_consistent_with_store() {
    let env = install_single_shard_federation();
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "adr0023_create_edge_index",
    );

    const EDGE_QUERY: &str = "MATCH (a)-[e:KNOWS {weight: 7}]->(b) RETURN e";

    let _ = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person)-[:KNOWS {weight: 7}]->(:Project)",
        "adr0023_pre_upgrade_edge_insert",
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    let before = gql_query_as_admin(&env, EDGE_QUERY);
    assert_eq!(
        before.row_count, 1,
        "pre-upgrade indexed edge query must find the single matching edge"
    );

    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");

    let _ = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person)-[:KNOWS {weight: 7}]->(:Project)",
        "adr0023_post_upgrade_edge_insert",
    );
    drain_maintenance_via_timer(&env, env.graph_source);

    let after = gql_query_as_admin(&env, EDGE_QUERY);
    assert_eq!(
        after.row_count, 2,
        "post-upgrade indexed edge write must be visible through the index \
         (INV across the upgrade boundary for edges)"
    );
}

/// ADR 0023 D6 / P7: `DROP INDEX` must purge the dropped property's postings from
/// graph-index. Pre-D6 the router only cleared its catalog, orphaning the
/// postings on the index canister.
#[test]
fn drop_index_purges_postings_from_graph_index() {
    let env = install_single_shard_federation();
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "p7_create_index",
    );
    let _ = gql_execute_idempotent_as_admin(&env, "INSERT (:Person {age: 5})", "p7_insert");
    drain_maintenance_via_timer(&env, env.graph_source);

    let age_value = value_to_index_key_bytes(&Value::Int64(5))
        .expect("encode age value")
        .expect("age value is indexable");

    assert_eq!(
        count_postings_for_value(&env, &age_value),
        1,
        "the indexed write must create exactly one posting on graph-index"
    );

    drop_vertex_property_index(&env, INDEX_AGE_NAME, false, "p7_drop_index");

    assert_eq!(
        count_postings_for_value(&env, &age_value),
        0,
        "DROP INDEX must purge the dropped property's postings from graph-index \
         (P7: dropped indexes used to orphan their postings)"
    );
}

/// The single edge target bound to `b` by an index-served equality query that is
/// expected to return exactly one row. Resolving the row reads the edge at the
/// slot the index posting points to, so a stale (un-re-keyed) posting surfaces as
/// either a missing row or the wrong target here.
fn unique_edge_target(
    env: &FederationEnv,
    query: &str,
    key: &ElementIdEncodingKey,
) -> GlobalVertexId {
    let result: GqlQueryResult = gql_query_as_admin(env, query);
    assert_eq!(
        result.row_count, 1,
        "expected exactly one row for `{query}`"
    );
    let wire = IcWirePlanQueryResult::decode_blob(
        result
            .rows_blob
            .as_ref()
            .expect("rows_blob for ELEMENT_ID query"),
    )
    .expect("decode rows_blob");
    let row = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row");
    let Value::Bytes(id_bytes) = row.get("bid").expect("bid column") else {
        panic!("expected bid bytes, got {:?}", row.get("bid"));
    };
    GraphPathVertexId::try_from_slice(id_bytes.as_ref())
        .expect("decode vertex id")
        .decode_global(key)
}

/// ADR 0023 verification item 3: timer-driven compaction across the upgrade
/// boundary. A `CompactVertexEdgeSpan` left in the shard's stable deferred queue
/// must survive a graph-shard upgrade, then be drained by the re-armed wasm
/// maintenance timer — whose async tick fetches the router catalog, runs the
/// LARA compaction (which **moves** the surviving edges' `slot_index`), and
/// flushes the re-keyed edge postings in the same tick (P2). After the timer
/// drains, index-served edge equality lookups must still resolve to the correct
/// targets (INV holds: postings track the re-keyed slots, no stale/orphan slots).
///
/// The move re-keys three sidecars off the same `EdgeSlotMove`: the edge index
/// postings (forward index-served lookups, above), the edge-alias canonical
/// target, and the property sidecar (`EDGE_PROPERTIES`). The alias + property
/// sidecars are exercised by reading each surviving edge's weight through the
/// reverse in-edge -> alias -> canonical path, which resolves at the moved slot.
#[test]
fn timer_compaction_after_upgrade_rekeys_edge_postings_consistently() {
    let env = install_single_shard_federation();
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "adr0023_timer_create_edge_index",
    );
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);

    // One source with three KNOWS out-edges (slots 0, 1, 2) carrying distinct
    // indexed weights. Deleting slot 0 then compacting moves slots 1 -> 0 and
    // 2 -> 1, which is the only operation that changes an edge's `slot_index`.
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target_a = e2e_insert_vertex(&env, env.graph_source);
    let target_b = e2e_insert_vertex(&env, env.graph_source);
    let target_c = e2e_insert_vertex(&env, env.graph_source);
    for (target, value) in [(&target_a, 10), (&target_b, 20), (&target_c, 30)] {
        e2e_insert_directed_edge_with_property(
            &env,
            env.graph_source,
            source.local_vertex_id,
            target.local_vertex_id,
            knows.raw(),
            weight.raw(),
            value,
        );
    }

    let key = ElementIdEncodingKey(
        gleaph_pocket_ic_tests::federation_graph_element_id_encoding_key_bytes(&env),
    );

    // Delete the slot-0 edge (weight 10): the posting is removed and a tombstone
    // is left at slot 0; weights 20/30 still resolve to B/C at their original
    // slots 1/2.
    e2e_delete_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target_a.local_vertex_id,
        weight.raw(),
    );
    assert_eq!(
        gql_query_as_admin(&env, "MATCH (a)-[e:KNOWS {weight: 10}]->(b) RETURN b").row_count,
        0,
        "deleted edge must be gone from the store and the index"
    );
    assert_eq!(
        unique_edge_target(
            &env,
            "MATCH (a)-[e:KNOWS {weight: 20}]->(b) RETURN ELEMENT_ID(b) AS bid",
            &key,
        ),
        target_b.global_vertex_id,
        "weight 20 must resolve to B before compaction"
    );
    // The same edge resolved from the reverse side (in-edge -> alias -> canonical)
    // reads its weight at the canonical forward slot 1 before any move.
    assert_eq!(
        e2e_reverse_resolved_edge_property(
            &env,
            env.graph_source,
            source.local_vertex_id,
            target_b.local_vertex_id,
            weight.raw(),
        ),
        Some(20),
        "weight 20 must resolve through the reverse alias path before compaction"
    );

    // Enqueue forward-span compaction WITHOUT an inline drain so the work is left
    // for the maintenance timer, then upgrade the shard before the timer fires.
    e2e_enqueue_forward_compaction(&env, env.graph_source, source.local_vertex_id);
    assert!(
        e2e_maintenance_queue_len(&env, env.graph_source) > 0,
        "enqueue-only compaction must leave deferred work for the timer"
    );

    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");
    assert!(
        e2e_maintenance_queue_len(&env, env.graph_source) > 0,
        "the deferred compaction must survive the upgrade in the stable queue"
    );

    // Fire the re-armed timer: its async tick compacts the span (moving B/C
    // slots) and flushes the re-keyed edge postings in-tick.
    drain_maintenance_via_timer(&env, env.graph_source);

    // INV after timer compaction + upgrade: each surviving weight resolves to the
    // correct target through its re-keyed posting, and the deleted weight is gone.
    assert_eq!(
        unique_edge_target(
            &env,
            "MATCH (a)-[e:KNOWS {weight: 20}]->(b) RETURN ELEMENT_ID(b) AS bid",
            &key,
        ),
        target_b.global_vertex_id,
        "weight 20 must still resolve to B after the timer re-keyed its posting"
    );
    assert_eq!(
        unique_edge_target(
            &env,
            "MATCH (a)-[e:KNOWS {weight: 30}]->(b) RETURN ELEMENT_ID(b) AS bid",
            &key,
        ),
        target_c.global_vertex_id,
        "weight 30 must still resolve to C after the timer re-keyed its posting"
    );
    assert_eq!(
        gql_query_as_admin(&env, "MATCH (a)-[e:KNOWS {weight: 10}]->(b) RETURN b").row_count,
        0,
        "the deleted edge must not reappear through a stale posting"
    );
    assert_eq!(
        gql_query_as_admin(&env, "MATCH (a)-[e:KNOWS]->(b) RETURN e").row_count,
        2,
        "exactly the two surviving edges remain in the store after compaction"
    );

    // INV for the alias/property sidecars that ride the SAME EdgeSlotMove as the
    // index postings: forward compaction re-keys the edge-alias canonical target
    // (`move_canonical_target`) and physically moves the property sidecar
    // (`EDGE_PROPERTIES`) to the new forward slot. Reading each surviving edge's
    // weight through the reverse in-edge -> alias -> canonical path must still
    // return the correct value at the *moved* slot. A stale alias would resolve to
    // a sibling's slot (returning the wrong weight) and an un-moved sidecar would
    // return nothing, so this catches a re-key gap the forward index lookup cannot.
    assert_eq!(
        e2e_reverse_resolved_edge_property(
            &env,
            env.graph_source,
            source.local_vertex_id,
            target_b.local_vertex_id,
            weight.raw(),
        ),
        Some(20),
        "weight 20 must resolve through the reverse alias path after the slot moved 1 -> 0"
    );
    assert_eq!(
        e2e_reverse_resolved_edge_property(
            &env,
            env.graph_source,
            source.local_vertex_id,
            target_c.local_vertex_id,
            weight.raw(),
        ),
        Some(30),
        "weight 30 must resolve through the reverse alias path after the slot moved 2 -> 1"
    );
}

/// A failed graph-index batch is persisted to the Graph repair journal. The journal must survive
/// a Graph upgrade and replay after the index becomes available again; retrying the canonical
/// mutation must not be required to restore the derived posting.
#[test]
fn repair_journal_replays_index_batch_after_graph_upgrade() {
    let env = install_single_shard_federation();
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "adr0023_repair_upgrade_create_index",
    );

    env.pic
        .stop_canister(env.index, None)
        .expect("stop graph-index to force a deferred posting flush");
    let outcome = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person {age: 99})",
        "adr0023_repair_upgrade_insert",
    );
    assert_eq!(
        outcome.row_count, 0,
        "the canonical insert commits while its index flush is deferred"
    );

    env.pic
        .start_canister(env.index, None)
        .expect("restart graph-index for repair replay");
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard with durable repair journal");
    // `drain_maintenance_via_timer` intentionally watches only the stable compaction queue. This
    // case has a repair journal but no compaction item, so advance the re-armed repair timer here.
    for _ in 0..12 {
        env.pic.advance_time(std::time::Duration::from_secs(2));
        for _ in 0..12 {
            env.pic.tick();
        }
    }

    let result = gql_query_as_admin(&env, "MATCH (n:Person {age: 99}) RETURN n");
    assert_eq!(
        result.row_count, 1,
        "repair journal replay after Graph upgrade must restore the index posting"
    );
}
