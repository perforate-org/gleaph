//! PocketIC: direct Graph typed V1 bulk endpoint and idempotent journal replay.
//!
//! This test exercises `execute_plan_update_batch_typed_v1` directly on a graph shard,
//! without Router activation (Plan 0110 keeps the Router typed path dormant). It proves:
//!
//! - the endpoint accepts a POSTED-shaped typed batch with complete-row seeds;
//! - callers other than the configured Router are rejected;
//! - mutations commit and the journal marks the bulk group completed;
//! - canonical Post/POSTED state is visible after the first call and is not duplicated by replay;
//! - an ineligible plan is rejected without requiring a second canister fixture.

use candid::{Decode, Encode};
use gleaph_gql::ast::Expr;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn};
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::entry::EdgeInlineValueProfile;
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanBatchMode, ExecutePlanBatchResult, ExecutePlanBatchTypedArgs,
    ExecutePlanBatchTypedShared, ExecutePlanTypedOp, GetMutationJournalEntriesArgs,
    GetMutationJournalEntriesResult, MutationJournalState, ResolvedEdgeLabel, ResolvedLabelTable,
    ResolvedVertexLabel, SeedBindingsWire, SeedRowWire, SeedVertexBinding,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, SOURCE_SHARD, admin_intern_edge_label, admin_intern_vertex_label,
    e2e_insert_vertex_with_label, federation_graph_element_id_encoding_key_bytes,
    gql_query_as_admin, install_single_shard_federation, update_as_router,
};
use std::rc::Rc;

fn posted_plan_bundle() -> Vec<u8> {
    let plan = PhysicalPlan::from_ops(vec![
        PlanOp::NodeScan {
            variable: Rc::from("u"),
            label: Some("User".into()),
            property_projection: None,
        },
        PlanOp::InsertVertex {
            variable: Some(Rc::from("p")),
            labels: vec!["Post".into()],
            properties: vec![],
        },
        PlanOp::InsertEdge {
            variable: None,
            src: Rc::from("u"),
            dst: Rc::from("p"),
            direction: gleaph_gql::types::EdgeDirection::PointingRight,
            labels: vec!["POSTED".into()],
            properties: vec![],
        },
    ]);
    encode_block_plans(std::slice::from_ref(&plan), true).expect("encode plan")
}

fn make_typed_batch_args(
    env: &FederationEnv,
    user_label_id: u16,
    post_label_id: u16,
    posted_edge_label_id: u16,
    user_local_vertex_id: u32,
    mutation_id: u64,
) -> ExecutePlanBatchTypedArgs {
    let seed_row = SeedRowWire {
        vertex_bindings: vec![SeedVertexBinding {
            variable: "u".into(),
            local_vertex_id: user_local_vertex_id,
            required_vertex_label_ids: vec![user_label_id],
        }],
        float64_bindings: vec![],
    };
    ExecutePlanBatchTypedArgs {
        shared: ExecutePlanBatchTypedShared {
            target_shard_id: SOURCE_SHARD,
            element_id_encoding_key: federation_graph_element_id_encoding_key_bytes(env),
            mutation_id,
            plan_blob: posted_plan_bundle(),
            resolved_labels: Some(ResolvedLabelTable {
                vertex: vec![
                    ResolvedVertexLabel {
                        name: "User".into(),
                        id: gleaph_graph_kernel::entry::VertexLabelId::from_raw(user_label_id),
                    },
                    ResolvedVertexLabel {
                        name: "Post".into(),
                        id: gleaph_graph_kernel::entry::VertexLabelId::from_raw(post_label_id),
                    },
                ],
                edge: vec![ResolvedEdgeLabel {
                    name: "POSTED".into(),
                    id: gleaph_graph_kernel::entry::EdgeLabelId::from_raw(posted_edge_label_id),
                    inline_value_profile: EdgeInlineValueProfile::no_inline_value(),
                    inline_schema: None,
                }],
            }),
            resolved_properties: None,
            indexed_properties: None,
        },
        operations: vec![ExecutePlanTypedOp {
            params_blob: vec![],
            seed: SeedBindingsWire {
                entries: vec![],
                rows: vec![seed_row],
                complete_prefix_rows: true,
            },
        }],
        batch_mode: ExecutePlanBatchMode::Dynamic,
    }
}

fn journal_entries(env: &FederationEnv, mutation_ids: &[u64]) -> GetMutationJournalEntriesResult {
    use candid::Decode;
    let bytes = env
        .pic
        .query_call(
            env.graph_source,
            env.router,
            "get_mutation_journal_entries",
            candid::Encode!(&GetMutationJournalEntriesArgs {
                mutation_ids: mutation_ids.to_vec(),
            })
            .expect("encode"),
        )
        .unwrap_or_else(|e| panic!("get_mutation_journal_entries: {e:?}"));
    Decode!(&bytes, GetMutationJournalEntriesResult).expect("decode get_mutation_journal_entries")
}

#[test]
fn graph_typed_batch_enforces_boundary_executes_and_replays_once() {
    let env = install_single_shard_federation();

    let user_label = admin_intern_vertex_label(&env, "User");
    let post_label = admin_intern_vertex_label(&env, "Post");
    let posted_edge_label = admin_intern_edge_label(&env, "POSTED");

    let user = e2e_insert_vertex_with_label(&env, env.graph_source, user_label.raw());
    let second_user = e2e_insert_vertex_with_label(&env, env.graph_source, user_label.raw());

    let mutation_id: u64 = 1001;
    let mut args = make_typed_batch_args(
        &env,
        user_label.raw(),
        post_label.raw(),
        posted_edge_label.raw(),
        user.local_vertex_id,
        mutation_id,
    );
    let mut second_op = args.operations[0].clone();
    second_op.seed.rows[0].vertex_bindings[0].local_vertex_id = second_user.local_vertex_id;
    args.operations.push(second_op);

    let unauthorized = env.pic.update_call(
        env.graph_source,
        env.admin,
        "execute_plan_update_batch_typed_v1",
        Encode!(&args).expect("encode unauthorized request"),
    );
    assert!(
        unauthorized.is_err(),
        "only the configured Router may call the typed Graph endpoint"
    );

    let first: ExecutePlanBatchResult = update_as_router(
        &env,
        env.graph_source,
        "execute_plan_update_batch_typed_v1",
        args.clone(),
    );
    assert_eq!(first.results.len(), 2, "first call must execute both ops");
    assert!(
        first.results.iter().all(Result::is_ok),
        "both operations must succeed: {:?}",
        first.results
    );
    assert_eq!(
        first.next_index, None,
        "one small op must finish in one call"
    );

    let journal = journal_entries(&env, &[mutation_id]);
    assert_eq!(journal.entries.len(), 1, "journal must contain one entry");
    let entry = journal.entries[0]
        .as_ref()
        .expect("journal entry must exist");
    assert_eq!(
        entry.state(),
        MutationJournalState::Completed,
        "bulk group must be durable as completed"
    );
    assert_eq!(entry.mutation_id(), mutation_id);

    let posts_after_first = gql_query_as_admin(&env, "MATCH (p:Post) RETURN p");
    assert_eq!(
        posts_after_first.row_count, 2,
        "the typed call must commit one canonical Post per distinct seed"
    );
    let posted_after_first =
        gql_query_as_admin(&env, "MATCH (u:User)-[:POSTED]->(p:Post) RETURN p");
    assert_eq!(
        posted_after_first.row_count, 2,
        "the typed call must commit one canonical POSTED edge per distinct seed"
    );

    // Replay: same mutation_id with a different seed value must still return success because
    // the journal is already completed. This proves the endpoint is idempotent at the Graph
    // journal level and does not re-execute the mutation segment.
    let mut replay_args = args;
    replay_args.operations[0].seed.rows[0].vertex_bindings[0].local_vertex_id =
        user.local_vertex_id + 1;
    let replay: ExecutePlanBatchResult = update_as_router(
        &env,
        env.graph_source,
        "execute_plan_update_batch_typed_v1",
        replay_args,
    );
    assert_eq!(replay.results.len(), 2, "replay must return both results");
    assert!(
        replay.results.iter().all(Result::is_ok),
        "replay results must be journal-backed successes: {:?}",
        replay.results
    );
    assert_eq!(
        replay
            .results
            .iter()
            .map(|result| result.as_ref().expect("replay success").row_count)
            .collect::<Vec<_>>(),
        first
            .results
            .iter()
            .map(|result| result.as_ref().expect("first success").row_count)
            .collect::<Vec<_>>(),
        "completed journal replay must preserve ordered row counts"
    );

    assert_eq!(
        gql_query_as_admin(&env, "MATCH (p:Post) RETURN p").row_count,
        2,
        "journal replay must not duplicate either canonical Post"
    );
    assert_eq!(
        gql_query_as_admin(&env, "MATCH (u:User)-[:POSTED]->(p:Post) RETURN p").row_count,
        2,
        "journal replay must not duplicate either canonical POSTED edge"
    );

    // A query-only plan (NodeScan + RETURN) is not admitted by the classifier.
    let query_plan = PhysicalPlan::from_ops(vec![
        PlanOp::NodeScan {
            variable: Rc::from("u"),
            label: Some("User".into()),
            property_projection: None,
        },
        PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::var("u"),
                alias: Some(Rc::from("u")),
            }],
            distinct: false,
        },
    ]);
    let query_plan_blob =
        encode_block_plans(std::slice::from_ref(&query_plan), false).expect("encode query plan");

    let seed_row = SeedRowWire {
        vertex_bindings: vec![SeedVertexBinding {
            variable: "u".into(),
            local_vertex_id: user.local_vertex_id,
            required_vertex_label_ids: vec![user_label.raw()],
        }],
        float64_bindings: vec![],
    };
    let args = ExecutePlanBatchTypedArgs {
        shared: ExecutePlanBatchTypedShared {
            target_shard_id: SOURCE_SHARD,
            element_id_encoding_key: federation_graph_element_id_encoding_key_bytes(&env),
            mutation_id: mutation_id + 1,
            plan_blob: query_plan_blob,
            resolved_labels: Some(ResolvedLabelTable {
                vertex: vec![ResolvedVertexLabel {
                    name: "User".into(),
                    id: gleaph_graph_kernel::entry::VertexLabelId::from_raw(user_label.raw()),
                }],
                edge: vec![],
            }),
            resolved_properties: None,
            indexed_properties: None,
        },
        operations: vec![ExecutePlanTypedOp {
            params_blob: vec![],
            seed: SeedBindingsWire {
                entries: vec![],
                rows: vec![seed_row],
                complete_prefix_rows: true,
            },
        }],
        batch_mode: ExecutePlanBatchMode::Dynamic,
    };

    let bytes = env
        .pic
        .update_call(
            env.graph_source,
            env.router,
            "execute_plan_update_batch_typed_v1",
            Encode!(&args).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("execute_plan_update_batch_typed_v1: {e:?}"));
    let result: Result<ExecutePlanBatchResult, String> =
        Decode!(&bytes, Result<ExecutePlanBatchResult, String>).expect("decode");
    let err = result.expect_err("query-only plan must be rejected");
    assert!(
        err.contains("eligibility") && err.contains("write-path"),
        "unexpected error: {err}"
    );
}
