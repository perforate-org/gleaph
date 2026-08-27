//! PocketIC: the CLI prepared-run transport across the process/canister boundary.
//!
//! Drives `RouterPreparedTransport::run_query` over a real IC-agent HTTP connection against a
//! PocketIC Router, proving the CLI's wire behavior matches a direct Router `prepared_query`
//! call: parameters encoded by the shared `gleaph-gql-params` encoder are accepted by the
//! Router, matching rows come back decodable and renderable, and an unknown operation
//! surfaces the Router `NotFound` verdict verbatim.
//!
//! Expected counts stated before running (per plan contract):
//! - 2 `#[test]` functions.
//! - Each test constructs one PocketIC environment (`install_single_shard_federation`):
//!   3 funded canister creations and exactly 3 installs (Router, Index, Graph shard).
//! - The CLI connects anonymously; execution is admitted through a bounded PUBLIC publication
//!   plus PUBLIC data-plane rows (ADR 0074 slice 3).

use std::collections::BTreeMap;

use gleaph_cli::prepared::{
    RouterPreparedTransport, encode_run_params, parse_run_params, render_rows_table, run,
};
use gleaph_gql_ic::{GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::plan_exec::ReadMode;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, gql_mutate_as_admin, install_single_shard_federation,
    prepare_batch_as_admin,
};
use gleaph_prepared_api::PreparedRegistration;

fn registration(name: &str, query: &str) -> PreparedRegistration {
    PreparedRegistration {
        name: name.into(),
        query: query.into(),
        metadata: None,
    }
}

fn seed_two_tags(env: &FederationEnv) {
    gql_mutate_as_admin(env, "INSERT (:Tag {tag: 'alpha'})", "cli-run-seed-alpha");
    gql_mutate_as_admin(env, "INSERT (:Tag {tag: 'beta'})", "cli-run-seed-beta");
}

/// Admit the anonymous CLI caller to the seeded `:Tag` rows and to the prepared operation
/// itself (ADR 0074: prepared execution is default-deny until bounded publication plus
/// PUBLIC data-plane rows exist).
///
/// The operation name deliberately avoids `-`: the ADR 0074 statement grammar lexes
/// unquoted identifiers without hyphens, so only hyphen-free registered names can be
/// published through `GRANT EXECUTE ON PREPARED QUERY <name>`.
fn publish_tag_rows_to_public(env: &FederationEnv, operation: &str) {
    let statements = [
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Tag TO PUBLIC"),
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Tag TO PUBLIC"),
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Tag {{ tag }} TO PUBLIC"),
        format!("GRANT EXECUTE ON PREPARED QUERY {operation} TO PUBLIC"),
    ];
    for (index, statement) in statements.iter().enumerate() {
        gql_mutate_as_admin(env, statement, &format!("cli-run-public-grant-{index}"));
    }
}

/// The exact transport the `gleaph prepared run` dispatch builds, connected over the
/// PocketIC HTTP gateway (`make_live`). A custom endpoint requires fetching the root key,
/// which is the PocketIC test root key here.
fn cli_transport(env: &mut FederationEnv) -> RouterPreparedTransport {
    let url = env.pic.make_live(None);
    RouterPreparedTransport::connect(&env.router.to_text(), url.as_str(), None, true, None)
        .expect("connect CLI prepared transport")
}

#[test]
fn run_query_returns_registered_rows_over_the_http_boundary() {
    let mut env = install_single_shard_federation();
    seed_two_tags(&env);
    prepare_batch_as_admin(
        &env,
        &[registration(
            "cliruntag",
            "MATCH (n:Tag) WHERE n.tag = $tag RETURN n.tag AS tag",
        )],
    )
    .expect("register cliruntag");
    publish_tag_rows_to_public(&env, "cliruntag");

    let mut transport = cli_transport(&mut env);
    // The parameter goes through the exact CLI parse/encode path; the Router's acceptance of
    // these bytes proves the shared-encoder wire agreement end to end.
    let params =
        encode_run_params(parse_run_params(&[r#"tag="alpha""#.to_owned()]).expect("parse --param"))
            .expect("encode params");
    let result =
        run("cliruntag", params, ReadMode::Eventual, &mut transport).expect("cli run over http");
    assert_eq!(result.row_count, 1);

    let blob = result.rows_blob.clone().expect("materialized rows blob");
    let wire = GqlWireRows::decode_blob(&blob).expect("decode rows");
    assert_eq!(wire.rows.len(), 1);
    let columns: BTreeMap<String, GqlWireValue> =
        wire.rows[0].columns.clone().into_iter().collect();
    assert_eq!(
        columns.get("tag").expect("tag column"),
        &GqlWireValue::Text("alpha".into()),
        "the filtered row must be exactly the requested document"
    );

    let table = render_rows_table(&result).expect("render table");
    assert!(
        table
            .lines()
            .nth(1)
            .is_some_and(|line| line.contains("alpha")),
        "the rendered table must contain the returned row: {table}"
    );
}

#[test]
fn run_query_surfaces_unknown_operation_not_found_verbatim() {
    let mut env = install_single_shard_federation();
    let mut transport = cli_transport(&mut env);
    let error = run("missing-op", Vec::new(), ReadMode::Eventual, &mut transport)
        .expect_err("an unregistered operation must fail");
    let rendered = error.to_string();
    assert!(
        rendered.contains(r#"not found: prepared query "missing-op""#),
        "the Router NotFound verdict must surface verbatim: {rendered}"
    );
}
