//! PocketIC: the Router prepared batch-registration API (ADR 0061).
//!
//! Covers batch registration with execution, all-or-nothing atomicity, batch limits,
//! duplicate-name rejection, Router-owned result-schema completion (with unmappable column
//! omission), and `get_prepared` read-back.

use candid::{Decode, Encode};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, install_single_shard_federation, list_prepared_as_admin,
    prepare_batch_as_admin, prepared_query_with_params_as,
};
use gleaph_prepared_api::{
    OperationKind, PreparedOperation, PreparedOperationRecord, PreparedRegistration, ResultSchema,
    SemanticType,
};

/// ADR 0061 batch bound; the Router rejects a batch above this many operations.
const MAX_PREPARED_BATCH: usize = 32;

fn registration(name: &str, query: &str) -> PreparedRegistration {
    PreparedRegistration {
        name: name.into(),
        query: query.into(),
        metadata: None,
    }
}

fn operation(name: &str) -> PreparedOperation {
    PreparedOperation {
        name: name.into(),
        description: None,
        kind: OperationKind::Query,
        parameters: vec![],
        result: ResultSchema { columns: vec![] },
        supports_consistency: false,
        supports_idempotency: false,
        allowed_sorts: vec![],
    }
}

/// `get_prepared` without the panic-on-rejection helper so tests can assert `NotFound`.
fn get_prepared(env: &FederationEnv, name: &str) -> Result<PreparedOperationRecord, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_prepared",
            Encode!(&name.to_string()).expect("encode get_prepared"),
        )
        .expect("get_prepared call");
    Decode!(&bytes, Result<PreparedOperationRecord, RouterError>).expect("decode get_prepared")
}

#[test]
fn batch_registers_multiple_operations_and_executes_them() {
    let env = install_single_shard_federation();
    let mut alpha = registration("batch-alpha", "MATCH (n) RETURN 'a' AS tag");
    alpha.metadata = Some(operation("batch-alpha"));
    let mut beta = registration("batch-beta", "MATCH (n) RETURN n AS name");
    beta.metadata = Some(operation("batch-beta"));
    let operations = vec![alpha, beta];
    prepare_batch_as_admin(&env, &operations).expect("batch registers");

    let manifest = list_prepared_as_admin(&env, GRAPH_NAME);
    let names: Vec<_> = manifest
        .operations
        .iter()
        .map(|op| op.name.as_str())
        .collect();
    assert!(names.contains(&"batch-alpha"));
    assert!(names.contains(&"batch-beta"));

    // Both operations execute through the normal prepared path against the empty graph.
    let alpha = prepared_query_with_params_as(&env, env.admin, "batch-alpha", Vec::new());
    assert_eq!(alpha.row_count, 0);
    let beta = prepared_query_with_params_as(&env, env.admin, "batch-beta", Vec::new());
    assert_eq!(beta.row_count, 0);
}

#[test]
fn batch_is_atomic_when_one_operation_fails() {
    let env = install_single_shard_federation();
    let operations = vec![
        registration("atomic-valid", "MATCH (n) RETURN 'ok' AS tag"),
        registration("atomic-broken", "this is not a prepared query"),
    ];
    let error = prepare_batch_as_admin(&env, &operations).expect_err("batch must fail");
    assert!(error.to_string().contains("prepared op 'atomic-broken'"));

    // All-or-nothing: the valid prefix must not be committed anywhere.
    let missing = get_prepared(&env, "atomic-valid").expect_err("valid prefix must not commit");
    assert!(matches!(missing, RouterError::NotFound(_)));
}

#[test]
fn batch_rejects_more_than_limit_operations() {
    let env = install_single_shard_federation();
    let operations: Vec<_> = (0..=MAX_PREPARED_BATCH)
        .map(|i| registration(&format!("limit-op-{i}"), "MATCH (n) RETURN 'x' AS tag"))
        .collect();
    let error = prepare_batch_as_admin(&env, &operations).expect_err("batch must exceed limit");
    assert!(error.to_string().contains("exceeds the limit"));
}

#[test]
fn batch_rejects_duplicate_names() {
    let env = install_single_shard_federation();
    let operations = vec![
        registration("dup-op", "MATCH (n) RETURN 'x' AS tag"),
        registration("dup-op", "MATCH (n) RETURN n"),
    ];
    let error = prepare_batch_as_admin(&env, &operations).expect_err("duplicate must fail");
    assert!(
        error
            .to_string()
            .contains("duplicate prepared operation name")
    );
}

#[test]
fn batch_completes_result_schema_and_omits_unmappable_columns() {
    let env = install_single_shard_federation();
    let mut scalar = registration("scalar-return", "MATCH (n) RETURN 'ok' AS tag");
    scalar.metadata = Some(operation("scalar-return"));
    let mut node = registration("node-return", "MATCH (n) RETURN n AS name");
    node.metadata = Some(operation("node-return"));
    prepare_batch_as_admin(&env, &[scalar, node]).expect("batch registers");

    let manifest = list_prepared_as_admin(&env, GRAPH_NAME);
    let scalar_op = manifest
        .operations
        .iter()
        .find(|op| op.name == "scalar-return")
        .expect("scalar operation in manifest");
    assert_eq!(scalar_op.result.columns.len(), 1);
    assert_eq!(scalar_op.result.columns[0].name, "tag");
    assert_eq!(
        scalar_op.result.columns[0].semantic_type,
        SemanticType::Text
    );
    let node_op = manifest
        .operations
        .iter()
        .find(|op| op.name == "node-return")
        .expect("node operation in manifest");
    assert!(
        node_op.result.columns.is_empty(),
        "unmappable node columns must be omitted, not fatal"
    );
}

#[test]
fn get_prepared_returns_stored_source_and_metadata() {
    let env = install_single_shard_federation();
    let mut op = registration("fetch-record", "MATCH (n) RETURN 'ok' AS tag");
    op.metadata = Some(operation("fetch-record"));
    prepare_batch_as_admin(&env, &[op]).expect("register");

    let record = get_prepared(&env, "fetch-record").expect("get_prepared");
    assert_eq!(record.query, "MATCH (n) RETURN 'ok' AS tag");
    let metadata = record.metadata.expect("completed metadata");
    assert_eq!(metadata.name, "fetch-record");
    assert_eq!(metadata.result.columns.len(), 1);
    assert_eq!(metadata.result.columns[0].name, "tag");

    let missing = get_prepared(&env, "not-registered").expect_err("missing must fail");
    assert!(matches!(missing, RouterError::NotFound(_)));
}
