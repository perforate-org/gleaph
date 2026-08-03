//! GAP-2026-07-17-001 plan-batch boundary acceptance: the Graph `execute_plan_update_batch`
//! dynamic path must complete instead of trapping at the 40B message limit, and its
//! derived-index drain must push the batch's property postings to the index canister.
//!
//! The canbench targets (`crates/graph/src/bench/plan_batch.rs`) bound the local tail costs
//! (adversarial single operation 32.98M instructions, response construction ≤ 581K) against the
//! 2B + 500M reserves; canbench cannot exercise the inter-canister drain, which is what this
//! test covers end-to-end. The Router dispatches a multi-statement mutation program as Dynamic
//! plan-batch chunks (at most 70 operations per chunk at the 500M seed estimate); each fully
//! committed chunk drains its accumulated property postings to the index canister, so a batch
//! with an indexed property exercises both the plan-batch loop and the drain repeatedly.

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::index::{PhysicalIndexId, PostingHit};
use gleaph_pocket_ic_tests::{
    create_vertex_property_index, ensure_property, gql_mutate_result_as_admin, gql_query_as_admin,
    install_single_shard_federation,
};

const INDEX_NAME: &str = "adr0060_person_age";
const BOUNDARY_OPS: usize = 300;

#[test]
fn plan_batch_with_indexed_properties_completes_without_trapping_and_drains() {
    let env = install_single_shard_federation();
    create_vertex_property_index(
        &env,
        INDEX_NAME,
        "Person",
        "age",
        "adr0060_create_person_age_index",
    );
    let age_property = ensure_property(&env, "age");

    // A multi-statement mutation program: the Router dispatches it as Dynamic plan-batch chunks
    // (70 operations per chunk at the 500M seed estimate), so this exercises ~5 chunks, each
    // committing and draining its property postings before the next chunk starts.
    let mut program = String::new();
    for index in 0..BOUNDARY_OPS {
        if index > 0 {
            program.push_str(" NEXT ");
        }
        program.push_str(&format!("INSERT (:Person {{age: {index}}})"));
    }
    let result = gql_mutate_result_as_admin(&env, &program, "adr0060_plan_batch_boundary");
    assert!(
        result.token.is_some(),
        "the plan-batch mutation must execute and issue a mutation token (no trap)"
    );

    // The inserted data is queryable (a sample through the equality filter on the indexed
    // property; the planner may serve it from the store or through the index).
    let query = gql_query_as_admin(&env, "MATCH (n:Person {age: 150}) RETURN n.age AS age");
    let rows_blob = query
        .rows_blob
        .as_ref()
        .expect("age query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode age rows");
    assert_eq!(wire.rows.len(), 1, "age 150 vertex must be inserted");
    let row = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row");
    assert_eq!(
        row.get("age"),
        Some(&Value::Int64(150)),
        "the inserted age value must be queryable"
    );

    // The per-chunk drains must have pushed every posting: read the index canister directly. The
    // first index created in a fresh federation owns physical index id 1 (the Router allocator
    // `init_next_physical_index_id` starts at 1).
    let age_key = value_to_index_key_bytes(&Value::Int64(150))
        .expect("encode age index key")
        .expect("Int64 is indexable");
    let physical_index_id = PhysicalIndexId::new(1).expect("first index physical id");
    let bytes = env
        .pic
        .query_call(
            env.index,
            env.router,
            "lookup_equal",
            Encode!(&physical_index_id, &age_property.raw(), &age_key)
                .expect("encode index lookup"),
        )
        .expect("index lookup after plan-batch drain");
    let hits = Decode!(&bytes, Vec<PostingHit>).expect("decode lookup hits");
    assert_eq!(
        hits.len(),
        1,
        "the drained posting must be served by the index: {hits:?}"
    );
}
