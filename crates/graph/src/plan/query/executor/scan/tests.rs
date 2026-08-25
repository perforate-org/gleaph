use super::super::test_support::*;
use crate::plan::query::executor::execute_plan_query_bindings_with_initial_rows;
use gleaph_gql::parser;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::{PlanBuildOptions, build_plan_with_schema_and_options};
use gleaph_graph_kernel::entry::EdgeInlinePropertyProfile;
use gleaph_graph_kernel::plan_exec::{
    EdgeOrderingPolicy, ResolvedEdgeLabel, ResolvedLabelTable, ResolvedVertexLabel,
};
use ic_stable_lara::labeled::LabeledOrientation;
use pollster;

/// Posting-key domain tags from `gleaph_gql::value_index_key` (private there; these tests
/// assert exact encoded bytes, so they pin the constants locally).
const TAG_NUMERIC: u8 = 2;
const TAG_TEXT: u8 = 6;
const TAG_TEMPORAL: u8 = 8;

/// Builds an execution context with a Router-resolved label table. Vertex labels must be declared
/// once `resolved_labels` is present (the executor stops using the host-test fallback), and each
/// edge label carries the given policy so `ORDER BY INSERTION(e)` tests exercise the declared
/// capability rather than the fail-closed path.
fn resolved_execution_ctx(
    vertices: &[&str],
    edges: &[(&str, EdgeOrderingPolicy)],
) -> GqlExecutionContext {
    GqlExecutionContext {
        resolved_labels: Some(ResolvedLabelTable {
            vertex: vertices
                .iter()
                .map(|name| ResolvedVertexLabel {
                    name: (*name).into(),
                    id: crate::test_labels::vertex_label_id_for_name(name),
                })
                .collect(),
            edge: edges
                .iter()
                .map(|(name, policy)| {
                    ResolvedEdgeLabel::new(
                        *name,
                        crate::test_labels::edge_label_id_for_name(name),
                        EdgeInlinePropertyProfile::no_inline_property(),
                    )
                    .with_ordering(*policy)
                })
                .collect(),
        }),
        ..GqlExecutionContext::default()
    }
}
#[test]
fn index_scan_skips_foreign_shard_hits_in_standalone_mode() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let _ = store
        .insert_vertex_named(["ForeignIndexScanSeed"], [("age", Value::Uint8(1))])
        .expect("register age property");
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(1),
        vertex_id: 42,
    });
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "age".into(),
        value: ScanValue::Literal(Value::Int64(5)),
        cmp: CmpOp::Eq,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("age"),
    ]);
    let rows = pollster::block_on(execute_plan_query_bindings(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute index scan");

    assert!(rows.is_empty());
}

/// Seed the mock with per-key equality hits for `uid` and build an IN-list
/// IndexScan plan projecting the bound vertex's `uid`.
fn inlist_scan_fixture(
    store: &GraphStore,
    elements: Vec<ScanValue>,
) -> (
    MockPropertyIndex,
    PhysicalPlan,
    u32,
    Vec<u8>,
    Vec<u8>,
    u32,
    u32,
) {
    let pid = crate::test_labels::property_id_for_name("uid").raw();
    let alice_bytes = value_to_index_key_bytes(&Value::Text("alice".into()))
        .unwrap()
        .unwrap();
    let bob_bytes = value_to_index_key_bytes(&Value::Text("bob".into()))
        .unwrap()
        .unwrap();

    let ada = store
        .insert_vertex_named(["InListScan"], [("uid", Value::Text("alice".into()))])
        .expect("insert alice");
    let bob = store
        .insert_vertex_named(["InListScan"], [("uid", Value::Text("bob".into()))])
        .expect("insert bob");
    let ada_id = u32::try_from(u64::from(ada)).expect("vertex id");
    let bob_id = u32::try_from(u64::from(bob)).expect("vertex id");

    let index = MockPropertyIndex::default();
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "uid".into(),
            value: ScanValue::InList(elements),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::Project {
            columns: vec![project(prop("n", "uid"), "uid")],
            distinct: false,
        },
    ]);
    (index, plan, pid, alice_bytes, bob_bytes, ada_id, bob_id)
}

#[test]
fn executes_inlist_index_scan_as_union_of_point_probes() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let (index, plan, pid, alice_bytes, bob_bytes, ada_id, bob_id) = inlist_scan_fixture(
        &store,
        vec![
            ScanValue::Literal(Value::Text("bob".into())),
            ScanValue::Literal(Value::Text("alice".into())),
        ],
    );
    // Distinct vertices live under distinct posting keys.
    let ada_hit = PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: ada_id,
    };
    let bob_hit = PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: bob_id,
    };
    index.set_equal_hits_for(pid, alice_bytes.clone(), vec![ada_hit]);
    index.set_equal_hits_for(pid, bob_bytes.clone(), vec![bob_hit]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("uid"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute in-list index scan");

    assert_eq!(text_column(&result, "uid"), vec!["alice", "bob"]);
    // Exactly one equality probe per element, concatenated in encoded payload
    // byte order (ADR 0081 §4), not in list order.
    let calls = index.equal_calls.borrow();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, pid);
    assert_eq!(calls[0].1, alice_bytes);
    assert_eq!(calls[1].1, bob_bytes);
}

#[test]
fn inlist_index_scan_deduplicates_hits_across_duplicate_elements() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let (index, plan, pid, alice_bytes, _bob_bytes, ada_id, _bob_id) = inlist_scan_fixture(
        &store,
        vec![
            ScanValue::Literal(Value::Text("alice".into())),
            ScanValue::Literal(Value::Text("alice".into())),
        ],
    );
    let ada_hit = PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: ada_id,
    };
    index.set_equal_hits_for(pid, alice_bytes.clone(), vec![ada_hit]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("uid"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute dedup in-list scan");

    // A vertex equal to two list elements binds exactly one row.
    assert_eq!(text_column(&result, "uid"), vec!["alice"]);
}

#[test]
fn inlist_index_scan_skips_null_elements_and_missing_parameter_fails_closed() {
    let store = GraphStore::new();
    configure_test_index(&store);
    // Null contributes no probe; the remaining literal still resolves.
    let (index, plan, pid, alice_bytes, _bob_bytes, ada_id, _bob_id) = inlist_scan_fixture(
        &store,
        vec![
            ScanValue::Literal(Value::Null),
            ScanValue::Literal(Value::Text("alice".into())),
        ],
    );
    let ada_hit = PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: ada_id,
    };
    index.set_equal_hits_for(pid, alice_bytes, vec![ada_hit]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("uid"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute null-element in-list scan");
    assert_eq!(text_column(&result, "uid"), vec!["alice"]);

    // A missing parameter element fails closed instead of narrowing the union.
    let (index, plan, _, _, _, _, _) = inlist_scan_fixture(
        &store,
        vec![
            ScanValue::Parameter("$who".into()),
            ScanValue::Literal(Value::Text("alice".into())),
        ],
    );
    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("missing parameter must fail closed");
    assert!(
        matches!(err, PlanQueryError::MissingParameter { ref name } if name == "$who"),
        "expected missing-parameter error, got {err:?}"
    );
}

#[test]
fn executes_equality_index_scan_with_sortable_key() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let vid = store
        .insert_vertex_named(["IndexScanEq"], [("age", Value::Uint8(5))])
        .expect("insert vertex");
    let pid = crate::test_labels::property_id_for_name("age").raw();
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "age".into(),
        value: ScanValue::Literal(Value::Int64(5)),
        cmp: CmpOp::Eq,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("age"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.equal_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(
        calls[0].1,
        value_to_index_key_bytes(&Value::Uint8(5)).unwrap().unwrap()
    );
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn equality_index_scan_unifies_decimal_and_integer_key_with_final_filter() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let price = gleaph_gql::types::Decimal::parse("5.00").expect("decimal");
    let vid = store
        .insert_vertex_named(["IndexScanDecimalEq"], [("price", Value::Decimal(price))])
        .expect("insert vertex");
    let pid = crate::test_labels::property_id_for_name("price").raw();
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "price".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "price")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(5)))),
            })],
            stage: 0,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("price"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute decimal equality index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.equal_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(
        calls[0].1,
        value_to_index_key_bytes(&Value::Decimal(price))
            .unwrap()
            .unwrap()
    );
}

#[test]
fn equality_index_scan_unifies_float_and_decimal_key_with_final_filter() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let bound = gleaph_gql::types::Decimal::parse("1.5").expect("decimal");
    let vid = store
        .insert_vertex_named(["IndexScanFloatEq"], [("score", Value::Float64(1.5))])
        .expect("insert vertex");
    let pid = crate::test_labels::property_id_for_name("score").raw();
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "score".into(),
            value: ScanValue::Literal(Value::Decimal(bound)),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "score")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(bound)))),
            })],
            stage: 0,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("score"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute float equality index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.equal_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(
        calls[0].1,
        value_to_index_key_bytes(&Value::Float64(1.5))
            .unwrap()
            .unwrap()
    );
}

#[test]
fn equality_index_scan_final_filter_drops_inexact_float_decimal_candidate() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let bound = gleaph_gql::types::Decimal::parse("0.1").expect("decimal");
    let vid = store
        .insert_vertex_named(["IndexScanFloatInexact"], [("score", Value::Float64(0.1))])
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "score".into(),
            value: ScanValue::Literal(Value::Decimal(bound)),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "score")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(bound)))),
            })],
            stage: 0,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("score"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute inexact float equality index scan");

    assert!(result.rows.is_empty());
}

#[test]
fn equality_index_scan_matches_list_valued_posting() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let stored = Value::List(vec![Value::Uint8(1), Value::Text("a".into())]);
    let bound = Value::List(vec![Value::Int64(1), Value::Text("a".into())]);
    let vid = store
        .insert_vertex_named(["IndexScanListEq"], [("tags", stored.clone())])
        .expect("insert vertex");
    let pid = crate::test_labels::property_id_for_name("tags").raw();
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "tags")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(bound))),
            })],
            stage: 0,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("tags"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute list equality index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.equal_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(
        calls[0].1,
        value_to_index_key_bytes(&stored).unwrap().unwrap()
    );
}

#[test]
fn equality_index_scan_matches_record_valued_posting_independent_of_field_order() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let stored = Value::Record(vec![
        ("b".into(), Value::Int64(2)),
        ("a".into(), Value::Int64(1)),
    ]);
    let bound = Value::Record(vec![
        ("a".into(), Value::Int64(1)),
        ("b".into(), Value::Int64(2)),
    ]);
    let vid = store
        .insert_vertex_named(["IndexScanRecordEq"], [("profile", stored.clone())])
        .expect("insert vertex");
    let pid = crate::test_labels::property_id_for_name("profile").raw();
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "profile".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "profile")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(bound))),
            })],
            stage: 0,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("profile"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute record equality index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.equal_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(
        calls[0].1,
        value_to_index_key_bytes(&stored).unwrap().unwrap()
    );
}

#[test]
fn equality_index_scan_final_filter_drops_inexact_nested_numeric_candidate() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let stored = Value::Record(vec![("score".into(), Value::Float64(0.1))]);
    let bound = Value::Record(vec![(
        "score".into(),
        Value::Decimal(gleaph_gql::types::Decimal::parse("0.1").expect("decimal")),
    )]);
    let vid = store
        .insert_vertex_named(["IndexScanRecordInexact"], [("profile", stored)])
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "profile".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "profile")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(bound))),
            })],
            stage: 0,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("profile"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute record inexact equality index scan");

    assert!(result.rows.is_empty());
}

#[test]
fn executes_range_index_scan_with_lookup_range() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let low = store
        .insert_vertex_named(["IndexScanRange"], [("age", Value::Int64(1))])
        .expect("insert low");
    let high = store
        .insert_vertex_named(["IndexScanRange"], [("age", Value::Int64(9))])
        .expect("insert high");
    let pid = crate::test_labels::property_id_for_name("age").raw();
    let index = MockPropertyIndex::default();
    index.range_hits.borrow_mut().extend([
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: u32::try_from(u64::from(low)).unwrap(),
        },
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: u32::try_from(u64::from(high)).unwrap(),
        },
    ]);
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "age".into(),
        value: ScanValue::Literal(Value::Int64(5)),
        cmp: CmpOp::Ge,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("age"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute range index scan");

    assert_eq!(result.rows.len(), 2);
    assert!(index.equal_calls.borrow().is_empty());
    let calls = index.range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    // The one-sided predicate pushes down as a Between interval clamped to the NUMERIC domain
    // (ceiling = tag NUMERIC + 1).
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: value_to_index_key_bytes(&Value::Int64(5)).unwrap().unwrap(),
            high: vec![TAG_NUMERIC + 1],
        }
    );
}

#[test]
fn executes_text_range_index_scan_with_domain_clamped_between() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let hit = store
        .insert_vertex_named(["IndexScanTextRange"], [("name", Value::Text("m".into()))])
        .expect("insert hit");
    let pid = crate::test_labels::property_id_for_name("name").raw();
    let index = MockPropertyIndex::default();
    index.range_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(hit)).unwrap(),
    });
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "name".into(),
        value: ScanValue::Literal(Value::Text("b".into())),
        cmp: CmpOp::Ge,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("name"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute text range index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: value_to_index_key_bytes(&Value::Text("b".into()))
                .unwrap()
                .unwrap(),
            high: vec![TAG_TEXT + 1],
        }
    );
}

/// Seed `name` postings for STARTS WITH scans and build a TextPrefix IndexScan plan.
fn prefix_scan_fixture(pattern: ScanValue) -> (MockPropertyIndex, PhysicalPlan, u32) {
    let pid = crate::test_labels::property_id_for_name("name").raw();
    let index = MockPropertyIndex::default();
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "name".into(),
            value: pattern,
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::Project {
            columns: vec![project(prop("n", "name"), "name")],
            distinct: false,
        },
    ]);
    (index, plan, pid)
}

#[test]
fn executes_text_prefix_index_scan_with_between_and_dedup() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let ada = store
        .insert_vertex_named(
            ["PrefixScan"],
            [("name", Value::Text("StrPred Ada".into()))],
        )
        .expect("insert ada");
    let ada_id = u32::try_from(u64::from(ada)).unwrap();
    let (index, plan, pid) = prefix_scan_fixture(ScanValue::TextPrefix(Box::new(
        ScanValue::Literal(Value::Text("Str".into())),
    )));
    // A duplicated posting must still bind exactly one row.
    index.range_hits.borrow_mut().extend([
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: ada_id,
        },
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: ada_id,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("name"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute prefix index scan");

    assert_eq!(text_column(&result, "name"), vec!["StrPred Ada"]);
    let calls = index.range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    // The pattern lowers to [TEXT tag] + pattern bytes, exclusive high adds the
    // 0xFF sentinel; UTF-8 payloads never contain 0xFF so every continuation is
    // below it.
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: vec![TAG_TEXT, b'S', b't', b'r'],
            high: vec![TAG_TEXT, b'S', b't', b'r', 255],
        }
    );
}

#[test]
fn executes_empty_pattern_prefix_scan_over_full_text_domain() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let hit = store
        .insert_vertex_named(["PrefixScanEmpty"], [("name", Value::Text("".into()))])
        .expect("insert empty");
    let (index, plan, _pid) = prefix_scan_fixture(ScanValue::TextPrefix(Box::new(
        ScanValue::Literal(Value::Text(String::new())),
    )));
    index.range_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(hit)).unwrap(),
    });

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("name"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute empty-prefix scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.range_calls.borrow();
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: vec![TAG_TEXT],
            high: vec![TAG_TEXT + 1],
        }
    );
}

#[test]
fn text_prefix_scan_missing_parameter_fails_closed() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let (index, plan, _pid) = prefix_scan_fixture(ScanValue::TextPrefix(Box::new(
        ScanValue::Parameter("$pre".into()),
    )));

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("name"),
    ]);
    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("missing parameter must fail closed");

    assert!(
        matches!(&err, PlanQueryError::MissingParameter { name } if name == "$pre"),
        "got: {err:?}"
    );
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn text_prefix_null_parameter_binds_no_rows_without_range_call() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(["PrefixScanNull"], [("name", Value::Text("x".into()))])
        .expect("insert");
    let (index, plan, _pid) = prefix_scan_fixture(ScanValue::TextPrefix(Box::new(
        ScanValue::Parameter("$pre".into()),
    )));

    let mut parameters = params();
    parameters.insert("pre".to_string(), Value::Null);
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("name"),
    ]);
    reset_node_scan_visits();
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("null pattern binds no rows");

    // NULL STARTS WITH anything is Unknown under three-valued logic: no rows,
    // no range lookup, and no node-scan fallback.
    assert!(result.rows.is_empty());
    assert!(index.range_calls.borrow().is_empty());
    assert_eq!(node_scan_visits(), 0);
}

#[test]
fn text_prefix_non_text_parameter_falls_back_to_node_scan() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(["PrefixScanFallback"], [("name", Value::Text("zz".into()))])
        .expect("insert");
    let (index, plan, _pid) = prefix_scan_fixture(ScanValue::TextPrefix(Box::new(
        ScanValue::Parameter("$pre".into()),
    )));

    let mut parameters = params();
    parameters.insert("pre".to_string(), Value::Int64(7));
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("name"),
    ]);
    reset_node_scan_visits();
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("non-TEXT pattern falls back to node scan");

    // The fallback scans every live vertex; the residual PropertyFilter of a
    // full plan would then enforce the predicate (and fail closed on non-TEXT
    // stored values). This manual plan has no residual filter, so the row
    // binding itself proves the fallback ran.
    assert!(index.range_calls.borrow().is_empty());
    assert_eq!(node_scan_visits(), u32::from(store.vertex_count()) as usize);
    assert_eq!(text_column(&result, "name"), vec!["zz"]);
}

/// Build a stats-driven plan for a parsed cypher query so the planner can fuse
/// a TEXT prefix anchor for range-indexed properties.
fn plan_query_with_table_stats(
    input: &str,
    stats: &gleaph_gql_planner::stats::TableStats,
) -> PhysicalPlan {
    let program = parser::parse(input).expect("parse");
    let tx = program
        .transaction_activity
        .expect("expected transaction activity");
    let block = tx.body.expect("expected statement block");
    let gleaph_gql::ast::Statement::Query(composite) = &block.first else {
        panic!("expected query statement");
    };
    assert!(composite.rest.is_empty(), "single statement expected");
    use gleaph_gql_integration::path_extension::GLEAPH_PATH_EXTENSION_HANDLER;
    build_plan_with_schema_and_options(
        &composite.left,
        PlanBuildOptions {
            stats: Some(stats),
            path_extensions: &GLEAPH_PATH_EXTENSION_HANDLER,
        },
        &NoSchema,
    )
    .expect("plan should build")
}

/// STARTS WITH e2e over parsed queries: index candidates come from an honest
/// interval simulation over the seeded names, then the residual filter decides.
#[test]
fn executes_cypher_startswith_pushdown_boundary_row_sets() {
    use gleaph_gql_planner::plan::ScanValue as SV;
    use gleaph_gql_planner::stats::TableStats;

    let store = GraphStore::new();
    configure_test_index(&store);
    // Boundary corpus: exact match, suffix extension, multibyte-ending pattern,
    // shorter diverging key, last-byte branching, and foreign-domain rows.
    let seeded = [
        ("StrPred Ada", "StrPred Ada"),
        ("StrPred", "StrPred"),
        ("Str", "Str"),
        ("Stq", "Stq"),
        ("日本", "日本"),
        ("日本語", "日本語"),
        ("日々", "日々"),
        ("Appl", "Appl"),
        ("Apple", "Apple"),
        ("Apply", "Apply"),
        ("App", "App"),
    ];
    let mut ids = BTreeMap::new();
    for (name, _) in seeded {
        let vid = store
            .insert_vertex_named(["PrefixE2E"], [("name", Value::Text(name.into()))])
            .expect("insert seed");
        ids.insert(
            name.to_string(),
            u32::try_from(u64::from(vid)).expect("vertex id"),
        );
    }

    let cases: Vec<(&str, ScanValue, Value, Vec<&str>)> = vec![
        // (query fragment pattern value, scan bound, param value, expected names)
        (
            "'Str'",
            SV::Literal(Value::Text("Str".into())),
            Value::Null,
            vec!["Str", "StrPred", "StrPred Ada"],
        ),
        (
            "'StrPred'",
            SV::Literal(Value::Text("StrPred".into())),
            Value::Null,
            vec!["StrPred", "StrPred Ada"],
        ),
        // Pattern ending on a multi-byte character: byte-successor logic would
        // wrongly exclude 日本語; the 0xFF sentinel keeps it in.
        (
            "'日本'",
            SV::Literal(Value::Text("日本".into())),
            Value::Null,
            vec!["日本", "日本語"],
        ),
        // Last-byte branching: Appl matches itself, Apple, and Apply, but not
        // the shorter diverging App.
        (
            "'Appl'",
            SV::Literal(Value::Text("Appl".into())),
            Value::Null,
            vec!["Appl", "Apple", "Apply"],
        ),
        (
            "$pre",
            SV::Parameter("$pre".into()),
            Value::Text("StrPred A".into()),
            vec!["StrPred Ada"],
        ),
    ];

    for (pattern_sql, bound, param_value, expected) in cases {
        let indexed_stats = {
            let mut stats = TableStats::default();
            stats.label_cardinality.insert("PrefixE2E".to_string(), 100);
            stats.range_indexed_vertex_properties.insert("name".into());
            stats
        };

        let where_clause = if param_value == Value::Null {
            format!("WHERE n.name STARTS WITH {pattern_sql}")
        } else {
            "WHERE n.name STARTS WITH $pre".to_string()
        };
        let input =
            format!("MATCH (n:PrefixE2E) {where_clause} RETURN n.name AS name ORDER BY name");

        let plan = plan_query_with_table_stats(&input, &indexed_stats);
        assert!(
            plan.ops.iter().any(|op| matches!(
                op,
                PlanOp::IndexScan {
                    value: SV::TextPrefix(_),
                    ..
                }
            )),
            "{input}: pushdown plan must contain a prefix IndexScan, got {:?}",
            plan.ops
        );

        // Honest index simulation: serve exactly the postings inside the encoded
        // prefix interval of this case's pattern value. The real posting walk is
        // ascending, so the simulated hits are inserted in encoded-key order.
        let pattern_value = match &bound {
            SV::Parameter(_) => param_value.clone(),
            other => crate::plan::query::executor::scan::index::resolve_scan_bound_value(
                other,
                &params(),
            )
            .expect("literal bound"),
        };
        let (low, high) = gleaph_gql::value_index_key::text_prefix_range_bounds(&pattern_value)
            .expect("text pattern interval");
        let index = MockPropertyIndex::default();
        let mut interval_hits: Vec<(Vec<u8>, PostingHit)> = Vec::new();
        for (name, _) in seeded {
            let key = value_to_index_key_bytes(&Value::Text(name.into()))
                .unwrap()
                .unwrap();
            if low.as_slice() <= key.as_slice() && key.as_slice() < high.as_slice() {
                interval_hits.push((
                    key.clone(),
                    PostingHit {
                        shard_id: ShardId::new(0),
                        vertex_id: ids[name],
                    },
                ));
            }
        }
        interval_hits.sort_by(|left, right| left.0.cmp(&right.0));
        for (_, hit) in interval_hits {
            index.range_hits.borrow_mut().push(hit);
        }

        let mut parameters = params();
        if param_value != Value::Null {
            parameters.insert("pre".to_string(), param_value.clone());
        }
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
            crate::test_labels::property_id_for_name("name"),
        ]);
        let pushed = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .unwrap_or_else(|err| panic!("{input}: pushdown execution failed: {err:?}"));

        // Red proof: without a range index the planner emits NodeScan + residual
        // PropertyFilter and the same rows must come back with no index client.
        let unindexed_stats = {
            let mut stats = TableStats::default();
            stats.label_cardinality.insert("PrefixE2E".to_string(), 100);
            stats
        };
        let residual_plan = plan_query_with_table_stats(&input, &unindexed_stats);
        assert!(
            !residual_plan
                .ops
                .iter()
                .any(|op| matches!(op, PlanOp::IndexScan { .. })),
            "{input}: unindexed plan must not contain an IndexScan"
        );
        let residual = pollster::block_on(execute_plan_query(
            &store,
            &residual_plan,
            &parameters,
            None,
            GqlExecutionContext::default(),
        ))
        .unwrap_or_else(|err| panic!("{input}: residual execution failed: {err:?}"));

        assert_eq!(
            text_column(&pushed, "name"),
            expected,
            "{input}: pushdown rows"
        );
        assert_eq!(
            text_column(&residual, "name"),
            expected,
            "{input}: residual-only rows must equal pushdown rows"
        );
    }
}

#[test]
fn executes_cypher_not_startswith_stays_residual_with_correct_rows() {
    use gleaph_gql_planner::stats::TableStats;

    let store = GraphStore::new();
    configure_test_index(&store);
    for name in ["Ada", "Bob"] {
        store
            .insert_vertex_named(["PrefixNotE2E"], [("name", Value::Text(name.into()))])
            .expect("insert seed");
    }
    let mut stats = TableStats::default();
    stats.label_cardinality.insert("PrefixNotE2E".into(), 10);
    stats.range_indexed_vertex_properties.insert("name".into());

    let input = "MATCH (n:PrefixNotE2E) WHERE NOT n.name STARTS WITH 'A' RETURN n.name AS name";
    let plan = plan_query_with_table_stats(input, &stats);
    assert!(
        !plan
            .ops
            .iter()
            .any(|op| matches!(op, PlanOp::IndexScan { .. })),
        "NOT STARTS WITH must never anchor, got: {:?}",
        plan.ops
    );
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        None,
        GqlExecutionContext::default(),
    ))
    .expect("execute NOT STARTS WITH");

    assert_eq!(text_column(&result, "name"), vec!["Bob"]);
}

#[test]
fn executes_datetime_range_index_scan_with_subtype_pinned_between() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let hit = store
        .insert_vertex_named(
            ["IndexScanDateTimeRange"],
            [("at", Value::DateTime(200, 0))],
        )
        .expect("insert hit");
    let pid = crate::test_labels::property_id_for_name("at").raw();
    let index = MockPropertyIndex::default();
    index.range_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(hit)).unwrap(),
    });
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "at".into(),
        value: ScanValue::Literal(Value::DateTime(100, 0)),
        cmp: CmpOp::Lt,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("at"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute datetime range index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    // Lt pins the interval to the DateTime subtype floor (tag TEMPORAL, subtype 4) and the raw bound.
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: vec![TAG_TEMPORAL, 4],
            high: value_to_index_key_bytes(&Value::DateTime(100, 0))
                .unwrap()
                .unwrap(),
        }
    );
}

#[test]
fn executes_edge_range_index_scan_with_domain_clamped_between() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let a = store
        .insert_vertex_named(["EdgeRangeA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["EdgeRangeB"], Vec::<(&str, Value)>::new())
        .expect("b");
    store
        .insert_directed_edge_named(a, b, Some("EdgeRangeRel"), [("weight", Value::Int64(9))])
        .expect("edge");
    let pid = crate::test_labels::property_id_for_name("weight").raw();
    let index = MockPropertyIndex::default();
    let plan = plan(vec![PlanOp::EdgeIndexScan {
        variable: "e".into(),
        property: "weight".into(),
        value: ScanValue::Literal(Value::Int64(5)),
        cmp: CmpOp::Ge,
        property_projection: None,
    }]);

    let _catalog = crate::test_labels::enter_indexed_edge_property_named("weight");
    pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute edge range index scan");

    // The one-sided predicate pushes down as a Between interval clamped to the NUMERIC domain
    // with no label sieve (endpoint binding owns label matching), mirroring the vertex scan.
    let calls = index.edge_range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(calls[0].2, None);
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: value_to_index_key_bytes(&Value::Int64(5)).unwrap().unwrap(),
            high: vec![TAG_NUMERIC + 1],
        }
    );
}

#[test]
fn edge_range_unsupported_domain_falls_back_to_store_scan_without_index_call() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let a = store
        .insert_vertex_named(["EdgeRangeFallA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["EdgeRangeFallB"], Vec::<(&str, Value)>::new())
        .expect("b");
    store
        .insert_directed_edge_named(
            a,
            b,
            Some("EdgeRangeFallRel"),
            [("weight", Value::Int64(9))],
        )
        .expect("first edge");
    let c = store
        .insert_vertex_named(["EdgeRangeFallC"], Vec::<(&str, Value)>::new())
        .expect("c");
    store
        .insert_directed_edge_named(
            a,
            c,
            Some("EdgeRangeFallRel"),
            [("weight", Value::Int64(3))],
        )
        .expect("second edge");

    let index = MockPropertyIndex::default();
    // Bool has no ordered comparison domain: the executor must not push down an open-ended or
    // clamped request; it scans the canonical EDGE_PROPERTIES superset instead and the plan's
    // residual filter decides exact matches.
    let plan = plan(vec![PlanOp::EdgeIndexScan {
        variable: "e".into(),
        property: "weight".into(),
        value: ScanValue::Literal(Value::Bool(true)),
        cmp: CmpOp::Gt,
        property_projection: None,
    }]);

    let _catalog = crate::test_labels::enter_indexed_edge_property_named("weight");
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute edge range fallback scan");

    assert!(index.edge_range_calls.borrow().is_empty());
    assert_eq!(result.rows.len(), 2, "superset candidates bind both edges");
}

/// Seed fixture for edge STARTS WITH scans: one source vertex with named edges
/// covering every boundary class (exact pattern, extension, short non-match,
/// multibyte-ending pattern, multibyte diverging sibling, last-byte branch).
fn edge_prefix_store() -> (GraphStore, Vec<String>) {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["EdgePrefixSrc"], Vec::<(&str, Value)>::new())
        .expect("a");
    let mut names = Vec::new();
    for name in ["Str", "Straw", "St", "日本語", "日々", "Apple"] {
        let b = store
            .insert_vertex_named(["EdgePrefixDst"], [("name", Value::Text(name.into()))])
            .expect("b vertex");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("EdgePrefixRel"),
                [("name", Value::Text(name.into()))],
            )
            .expect("named edge");
        names.push(name.to_string());
    }
    (store, names)
}

/// Hand-built leading edge text-prefix scan binding `b` and projecting its name.
fn edge_prefix_scan_plan(pattern: ScanValue, cmp: CmpOp) -> PhysicalPlan {
    plan(vec![
        PlanOp::EdgeIndexScan {
            variable: "e".into(),
            property: "name".into(),
            value: pattern,
            cmp,
            property_projection: None,
        },
        PlanOp::EdgeBindEndpoints {
            edge: "e".into(),
            near: "__anon_near".into(),
            far: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("EdgePrefixRel".into()),
            near_property_projection: None,
            far_property_projection: None,
            hop_aux_binding: None,
        },
        PlanOp::Project {
            columns: vec![project(prop("b", "name"), "name")],
            distinct: false,
        },
    ])
}

#[test]
fn executes_edge_text_prefix_index_scan_with_exact_between_bytes() {
    let store = GraphStore::new();
    let pid = crate::test_labels::property_id_for_name("name").raw();
    let index = MockPropertyIndex::default();
    let pattern = ScanValue::TextPrefix(Box::new(ScanValue::Literal(Value::Text("Str".into()))));
    let plan = edge_prefix_scan_plan(pattern, CmpOp::Eq);

    let _catalog = crate::test_labels::enter_indexed_edge_property_named("name");
    pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute edge text-prefix index scan");

    // The fused STARTS WITH lowers into the half-open encoded TEXT interval
    // [stripped pattern key, key + [0xFF]) with no label sieve (endpoint binding
    // owns label matching), mirroring the vertex prefix contract.
    let calls = index.edge_range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(calls[0].2, None);
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: vec![TAG_TEXT, b'S', b't', b'r'],
            high: vec![TAG_TEXT, b'S', b't', b'r', 0xFF],
        }
    );
}

#[test]
fn executes_empty_pattern_edge_prefix_scan_over_full_text_domain() {
    let _weight_guard = crate::test_labels::enter_indexed_edge_property_named("name");
    let (store, _) = edge_prefix_store();
    // '' spans the whole TEXT domain: every TEXT-named edge is a candidate and
    // the residual filter (absent from this hand-built plan) would keep them all.
    let pattern = ScanValue::TextPrefix(Box::new(ScanValue::Literal(Value::Text(String::new()))));
    let plan = edge_prefix_scan_plan(pattern, CmpOp::Eq);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute empty-pattern edge prefix scan");

    assert_eq!(
        sorted_names(&result),
        vec![
            "Apple".to_string(),
            "St".to_string(),
            "Str".to_string(),
            "Straw".to_string(),
            "日々".to_string(),
            "日本語".to_string(),
        ]
    );
}

#[test]
fn edge_text_prefix_missing_parameter_fails_closed() {
    let _guard = crate::test_labels::enter_indexed_edge_property_named("name");
    let (store, _) = edge_prefix_store();
    let pattern = ScanValue::TextPrefix(Box::new(ScanValue::Parameter("$pre".into())));
    let plan = edge_prefix_scan_plan(pattern, CmpOp::Eq);

    let err = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect_err("missing prefix parameter must fail closed");
    assert!(
        matches!(err, PlanQueryError::MissingParameter { ref name } if name == "$pre"),
        "expected missing-parameter error, got {err:?}"
    );
}

#[test]
fn edge_text_prefix_null_parameter_binds_no_rows_without_range_call() {
    let _guard = crate::test_labels::enter_indexed_edge_property_named("name");
    let (store, _) = edge_prefix_store();
    let pattern = ScanValue::TextPrefix(Box::new(ScanValue::Parameter("$pre".into())));
    let plan = edge_prefix_scan_plan(pattern, CmpOp::Eq);
    let mut params_map = std::collections::BTreeMap::new();
    params_map.insert("pre".to_string(), Value::Null);

    let index = MockPropertyIndex::default();
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params_map,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute null-pattern edge prefix scan");

    // STARTS WITH NULL is Unknown under three-valued logic: no rows, and the
    // interval lookup must not run.
    assert!(result.rows.is_empty());
    assert!(index.edge_range_calls.borrow().is_empty());
}

#[test]
fn edge_text_prefix_non_text_parameter_falls_back_to_superset_without_range_call() {
    let _guard = crate::test_labels::enter_indexed_edge_property_named("name");
    let (store, _) = edge_prefix_store();
    let pattern = ScanValue::TextPrefix(Box::new(ScanValue::Parameter("$pre".into())));
    let plan = edge_prefix_scan_plan(pattern, CmpOp::Eq);
    let mut params_map = std::collections::BTreeMap::new();
    params_map.insert("pre".to_string(), Value::Int64(5));

    let index = MockPropertyIndex::default();
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params_map,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute non-text edge prefix fallback");

    // A non-TEXT resolved pattern cannot form a TEXT interval: no pushdown, the
    // canonical-store superset feeds the residual filter (absent here, so every
    // candidate binds).
    assert!(index.edge_range_calls.borrow().is_empty());
    assert_eq!(result.rows.len(), 6, "superset candidates bind all edges");
}

#[test]
fn executes_edge_text_prefix_parameter_pattern_with_exact_between_bytes() {
    let store = GraphStore::new();
    let pid = crate::test_labels::property_id_for_name("name").raw();
    let index = MockPropertyIndex::default();
    let pattern = ScanValue::TextPrefix(Box::new(ScanValue::Parameter("$pfx".into())));
    let plan = edge_prefix_scan_plan(pattern, CmpOp::Eq);
    let mut params_map = std::collections::BTreeMap::new();
    params_map.insert("pfx".to_string(), Value::Text("Str".into()));

    let _catalog = crate::test_labels::enter_indexed_edge_property_named("name");
    pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params_map,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute parameterized edge text-prefix index scan");

    // The resolved parameter must realize the same encoded interval as the
    // literal form: [stripped TEXT key, key + [0xFF]).
    let calls = index.edge_range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(calls[0].2, None);
    assert_eq!(
        calls[0].1,
        PostingRangeRequest::Between {
            low: vec![TAG_TEXT, b'S', b't', b'r'],
            high: vec![TAG_TEXT, b'S', b't', b'r', 0xFF],
        }
    );
}

/// True when `expr` is exactly `var.property STARTS WITH <anything>` (mirror of the
/// planner-side helper for residual assertions on parsed plans).
fn is_edge_startswith_residual_on(expr: &gleaph_gql::ast::Expr, var: &str, property: &str) -> bool {
    let gleaph_gql::ast::ExprKind::StringPredicate {
        expr: lhs,
        kind: gleaph_gql::ast::StringPredicateKind::StartsWith,
        negated: false,
        ..
    } = &expr.kind
    else {
        return false;
    };
    let gleaph_gql::ast::ExprKind::PropertyAccess {
        expr: inner,
        property: prop,
    } = &lhs.kind
    else {
        return false;
    };
    matches!(&inner.kind, gleaph_gql::ast::ExprKind::Variable(name) if name == var)
        && prop == property
}

#[test]
fn executes_cypher_edge_surplus_startswith_conjunct_filters_through_residual() {
    let _guard = crate::test_labels::enter_indexed_edge_property_named("name");
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["EdgePrefixSrc"], Vec::<(&str, Value)>::new())
        .expect("a");
    for (name, tag) in [("Straw", "y-keep"), ("Stream", "n-skip"), ("St", "yes-too")] {
        let b = store
            .insert_vertex_named(["EdgePrefixDst"], [("name", Value::Text(name.into()))])
            .expect("b vertex");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("EdgePrefixRel"),
                [
                    ("name", Value::Text(name.into())),
                    ("tag", Value::Text(tag.into())),
                ],
            )
            .expect("named edge");
    }

    // Only `name` is indexed: `name STARTS WITH 'Str'` anchors while
    // `tag STARTS WITH 'y'` must survive as a residual PropertyFilter. The row
    // set discriminates all three outcomes: pushdown-only would bind Straw and
    // Stream, residual-only would bind everything.
    let input = "MATCH (a:EdgePrefixSrc)-[e:EdgePrefixRel]->(b:EdgePrefixDst) WHERE e.name STARTS WITH 'Str' AND e.tag STARTS WITH 'y' RETURN b.name";
    let plan = plan_with_optional_edge_index_stats(input, Some("name"));
    assert!(
        plan.ops.iter().any(|op| matches!(
            op,
            PlanOp::EdgeIndexScan {
                property,
                value: ScanValue::TextPrefix(_),
                cmp: CmpOp::Eq,
                ..
            } if &**property == "name"
        )),
        "indexed conjunct must anchor, got: {:?}",
        plan.ops
    );
    assert!(
        plan.ops.iter().any(|op| matches!(
            op,
            PlanOp::PropertyFilter { predicates, .. }
                if predicates
                    .iter()
                    .any(|p| is_edge_startswith_residual_on(p, "e", "tag"))
        )),
        "unanchored conjunct must remain residual, got: {:?}",
        plan.ops
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute surplus-conjunct edge prefix query");
    assert_eq!(text_column(&result, "b.name"), vec!["Straw".to_string()]);
}

/// Parses a Cypher-dialect query with an optionally-indexed edge property so the
/// planner can select the leading edge index anchor; passing `None` yields the
/// unindexed plan used as the pushdown red-proof twin.
fn plan_with_optional_edge_index_stats(input: &str, indexed_prop: Option<&str>) -> PhysicalPlan {
    let mut stats = gleaph_gql_planner::TableStats::default();
    if let Some(prop) = indexed_prop {
        stats.indexed_edge_properties.insert(prop.to_string());
    }
    let program = parser::parse(input).unwrap_or_else(|err| panic!("parse error: {err}"));
    let tx = program
        .transaction_activity
        .expect("expected transaction activity");
    let block = tx.body.expect("expected statement block");
    let Statement::Query(composite) = &block.first else {
        panic!("expected a query statement, got {input:?}");
    };
    assert!(composite.rest.is_empty(), "unexpected set operation");
    build_plan_with_schema_and_options(
        &composite.left,
        PlanBuildOptions {
            stats: Some(&stats),
            path_extensions: &gleaph_gql_integration::path_extension::GLEAPH_PATH_EXTENSION_HANDLER,
        },
        &NoSchema,
    )
    .expect("plan should build")
}

#[test]
fn executes_cypher_edge_startswith_pushdown_boundary_row_sets() {
    let _guard = crate::test_labels::enter_indexed_edge_property_named("name");
    let (store, _) = edge_prefix_store();

    let mut param_pre = std::collections::BTreeMap::new();
    param_pre.insert("pre".to_string(), Value::Text("Appl".into()));

    let bound_b = |result: &crate::plan::query::executor::PlanQueryResult| {
        let mut names = text_column(result, "b.name");
        names.sort();
        names
    };

    let run = |input: &str, params_map: &std::collections::BTreeMap<String, Value>| {
        let plan = plan_with_optional_edge_index_stats(input, Some("name"));
        assert!(
            plan.ops.iter().any(|op| matches!(
                op,
                PlanOp::EdgeIndexScan {
                    property,
                    value: ScanValue::TextPrefix(_),
                    cmp: CmpOp::Eq,
                    ..
                } if property.as_ref() == "name"
            )),
            "expected anchored prefix EdgeIndexScan for {input}, got {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, params_map, GqlExecutionContext::default())
            .unwrap_or_else(|err| panic!("execute {input}: {err:?}"));
        bound_b(&result)
    };

    // Inline pattern WHERE and path-level WHERE agree; boundaries cover the exact
    // pattern (binds itself plus extensions), a multibyte-ending pattern (binds its
    // extension but not the diverging sibling), and a last-byte-branching pattern
    // through a parameter.
    let inline = run(
        "MATCH (a:EdgePrefixSrc)-[e:EdgePrefixRel WHERE e.name STARTS WITH 'Str']->(b:EdgePrefixDst) RETURN b.name",
        &params(),
    );
    assert_eq!(
        inline,
        vec!["Str".to_string(), "Straw".to_string()],
        "'Str' binds itself and its extension"
    );
    let path_level = run(
        "MATCH (a:EdgePrefixSrc)-[e:EdgePrefixRel]->(b:EdgePrefixDst) WHERE e.name STARTS WITH '日本' RETURN b.name",
        &params(),
    );
    assert_eq!(
        path_level,
        vec!["日本語".to_string()],
        "multibyte-ending prefix must bind its extension but not 日々"
    );
    let parameterized = run(
        "MATCH (a:EdgePrefixSrc)-[e:EdgePrefixRel]->(b:EdgePrefixDst) WHERE e.name STARTS WITH $pre RETURN b.name",
        &param_pre,
    );
    assert_eq!(parameterized, vec!["Apple".to_string()]);

    // Red proof: with no indexed edge property the planner keeps everything
    // residual (no EdgeIndexScan), and the row sets are identical.
    let run_unindexed = |input: &str, params_map: &std::collections::BTreeMap<String, Value>| {
        let plan = plan_with_optional_edge_index_stats(input, None);
        assert!(
            !plan
                .ops
                .iter()
                .any(|op| matches!(op, PlanOp::EdgeIndexScan { .. })),
            "unindexed plan must not emit EdgeIndexScan, got {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, params_map, GqlExecutionContext::default())
            .unwrap_or_else(|err| panic!("execute unindexed {input}: {err:?}"));
        bound_b(&result)
    };
    assert_eq!(
        run_unindexed(
            "MATCH (a:EdgePrefixSrc)-[e:EdgePrefixRel WHERE e.name STARTS WITH 'Str']->(b:EdgePrefixDst) RETURN b.name",
            &params()
        ),
        inline,
        "pushdown removal must not change the row set"
    );
    assert_eq!(
        run_unindexed(
            "MATCH (a:EdgePrefixSrc)-[e:EdgePrefixRel]->(b:EdgePrefixDst) WHERE e.name STARTS WITH '日本' RETURN b.name",
            &params()
        ),
        path_level,
        "pushdown removal must not change the row set"
    );
    assert_eq!(
        run_unindexed(
            "MATCH (a:EdgePrefixSrc)-[e:EdgePrefixRel]->(b:EdgePrefixDst) WHERE e.name STARTS WITH $pre RETURN b.name",
            &param_pre
        ),
        parameterized,
        "pushdown removal must not change the row set"
    );
}

/// Store fixture for edge IN-list scans: `a` holds three edges to labeled `b`
/// vertices whose weights/names identify them (`w5`, `w7`, `w9`). Callers must
/// hold `crate::test_labels::enter_indexed_edge_property_named("weight")` for
/// the duration of the test.
fn edge_inlist_store() -> (GraphStore, Vec<String>) {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["EdgeInA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let mut names = Vec::new();
    for (name, weight) in [("w5", 5i64), ("w7", 7), ("w9", 9)] {
        let b = store
            .insert_vertex_named(["EdgeInB"], [("name", Value::Text(name.into()))])
            .expect("b vertex");
        store
            .insert_directed_edge_named(a, b, Some("EdgeInRel"), [("weight", Value::Int64(weight))])
            .expect("weighted edge");
        names.push(name.to_string());
    }
    (store, names)
}

/// Hand-built leading edge IN-list scan binding `b` and projecting its name.
fn edge_inlist_scan_plan(rel_label: &str, elements: Vec<ScanValue>, cmp: CmpOp) -> PhysicalPlan {
    plan(vec![
        PlanOp::EdgeIndexScan {
            variable: "e".into(),
            property: "weight".into(),
            value: ScanValue::InList(elements),
            cmp,
            property_projection: None,
        },
        PlanOp::EdgeBindEndpoints {
            edge: "e".into(),
            near: "__anon_near".into(),
            far: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some(rel_label.into()),
            near_property_projection: None,
            far_property_projection: None,
            hop_aux_binding: None,
        },
        PlanOp::Project {
            columns: vec![project(prop("b", "name"), "name")],
            distinct: false,
        },
    ])
}

fn sorted_names(result: &PlanQueryResult) -> Vec<String> {
    let mut names = text_column(result, "name");
    names.sort();
    names
}

#[test]
fn executes_edge_inlist_index_scan_as_union_of_point_probes() {
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let (store, _) = edge_inlist_store();
    let plan = edge_inlist_scan_plan(
        "EdgeInRel",
        vec![
            ScanValue::Literal(Value::Int64(7)),
            ScanValue::Literal(Value::Int64(5)),
        ],
        CmpOp::Eq,
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute edge in-list scan");

    assert_eq!(
        sorted_names(&result),
        vec!["w5".to_string(), "w7".to_string()]
    );
}

#[test]
fn edge_inlist_index_scan_deduplicates_hits_across_duplicate_elements() {
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["EdgeInDedupA"], Vec::<(&str, Value)>::new())
        .expect("a");
    for name in ["w5", "w5b"] {
        let b = store
            .insert_vertex_named(["EdgeInDedupB"], [("name", Value::Text(name.into()))])
            .expect("b vertex");
        store
            .insert_directed_edge_named(a, b, Some("EdgeInDedupRel"), [("weight", Value::Int64(5))])
            .expect("weighted edge");
    }
    // Two distinct edges match weight 5; duplicated elements must not multiply rows.
    let plan = edge_inlist_scan_plan(
        "EdgeInDedupRel",
        vec![
            ScanValue::Literal(Value::Int64(5)),
            ScanValue::Literal(Value::Int64(5)),
        ],
        CmpOp::Eq,
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute dedup edge in-list scan");

    assert_eq!(
        sorted_names(&result),
        vec!["w5".to_string(), "w5b".to_string()],
        "each matching edge binds exactly once"
    );
}

#[test]
fn edge_inlist_index_scan_skips_null_elements_and_missing_parameter_fails_closed() {
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let (store, _) = edge_inlist_store();
    // Null contributes no probe; the remaining literal still resolves.
    let plan = edge_inlist_scan_plan(
        "EdgeInRel",
        vec![
            ScanValue::Literal(Value::Null),
            ScanValue::Literal(Value::Int64(7)),
        ],
        CmpOp::Eq,
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute null-element edge in-list scan");
    assert_eq!(sorted_names(&result), vec!["w7".to_string()]);

    // A missing parameter element fails closed instead of narrowing the union.
    let plan = edge_inlist_scan_plan(
        "EdgeInRel",
        vec![
            ScanValue::Parameter("$who".into()),
            ScanValue::Literal(Value::Int64(7)),
        ],
        CmpOp::Eq,
    );
    let err = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect_err("missing parameter must fail closed");
    assert!(
        matches!(err, PlanQueryError::MissingParameter { ref name } if name == "$who"),
        "expected missing-parameter error, got {err:?}"
    );
}

#[test]
fn edge_inlist_with_non_equality_comparison_fails_closed() {
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let (store, _) = edge_inlist_store();
    let plan = edge_inlist_scan_plan(
        "EdgeInRel",
        vec![ScanValue::Literal(Value::Int64(5))],
        CmpOp::Lt,
    );

    let err = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect_err("non-equality IN-list bound must fail closed");
    assert!(
        matches!(err, PlanQueryError::UnsupportedOp(msg) if msg.contains("unexpanded IN-list")),
        "expected unexpanded IN-list error, got {err:?}"
    );
}

/// Parses a Cypher-dialect query with `weight` registered as an indexed edge
/// property so the planner can select the leading edge index anchor.
fn plan_with_edge_inlist_stats(input: &str) -> PhysicalPlan {
    let mut stats = gleaph_gql_planner::TableStats::default();
    stats.indexed_edge_properties.insert("weight".to_string());
    let program = parser::parse(input).unwrap_or_else(|err| panic!("parse error: {err}"));
    let tx = program
        .transaction_activity
        .expect("expected transaction activity");
    let block = tx.body.expect("expected statement block");
    let Statement::Query(composite) = &block.first else {
        panic!("expected a query statement, got {input:?}");
    };
    assert!(composite.rest.is_empty(), "unexpected set operation");
    build_plan_with_schema_and_options(
        &composite.left,
        PlanBuildOptions {
            stats: Some(&stats),
            path_extensions: &gleaph_gql_integration::path_extension::GLEAPH_PATH_EXTENSION_HANDLER,
        },
        &NoSchema,
    )
    .expect("plan should build")
}

#[test]
fn indexed_edge_inlist_queries_end_to_end_match_equality_semantics() {
    let store = GraphStore::new();
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let a = store
        .insert_vertex_named(["EdgeE2A"], Vec::<(&str, Value)>::new())
        .expect("a");
    for (name, weight) in [("w5", 5i64), ("w7", 7), ("w9", 9)] {
        let b = store
            .insert_vertex_named(["EdgeE2B"], [("name", Value::Text(name.into()))])
            .expect("b vertex");
        store
            .insert_directed_edge_named(a, b, Some("EdgeE2Rel"), [("weight", Value::Int64(weight))])
            .expect("weighted edge");
    }

    let run = |input: &str| {
        let plan = plan_with_edge_inlist_stats(input);
        // Both WHERE forms must lower into the anchored union-of-point-probes scan,
        // exactly like the single-value equality anchor they mirror.
        assert!(
            plan.ops.iter().any(|op| matches!(
                op,
                PlanOp::EdgeIndexScan {
                    property,
                    value: ScanValue::InList(_),
                    cmp: CmpOp::Eq,
                    ..
                } if property.as_ref() == "weight"
            )),
            "expected anchored EdgeIndexScan with InList bound for {input}, got {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .unwrap_or_else(|err| panic!("execute {input}: {err:?}"));
        let mut bound_b: Vec<String> = result
            .rows
            .iter()
            .map(|row| format!("{:?}", row.get("b")))
            .collect();
        bound_b.sort();
        bound_b
    };

    // Inline pattern WHERE and path-level WHERE agree with each other…
    let inline =
        run("MATCH (a:EdgeE2A)-[e:EdgeE2Rel WHERE e.weight IN [5, 7]]->(b:EdgeE2B) RETURN b");
    let path_level =
        run("MATCH (a:EdgeE2A)-[e:EdgeE2Rel]->(b:EdgeE2B) WHERE e.weight IN [5, 7] RETURN b");
    assert_eq!(inline.len(), 2, "weights 5 and 7 each bind one row");
    assert_eq!(inline, path_level);

    // …and a single-element IN matches exactly what its equality twin matches.
    let in_single =
        run("MATCH (a:EdgeE2A)-[e:EdgeE2Rel]->(b:EdgeE2B) WHERE e.weight IN [7] RETURN b");
    let equality_plan = plan_with_edge_inlist_stats(
        "MATCH (a:EdgeE2A)-[e:EdgeE2Rel]->(b:EdgeE2B) WHERE e.weight = 7 RETURN b",
    );
    let equality_result = store
        .execute_plan_query(&equality_plan, &params(), GqlExecutionContext::default())
        .expect("execute equality twin");
    let mut equality_bound_b: Vec<String> = equality_result
        .rows
        .iter()
        .map(|row| format!("{:?}", row.get("b")))
        .collect();
    equality_bound_b.sort();
    assert_eq!(in_single.len(), 1);
    assert_eq!(in_single, equality_bound_b);
}

/// GAP-2026-08-24-004 regression: a parsed anchored edge scan whose far endpoint
/// carries both a trailing `IsLabeled` residual and a property-level projection
/// must return the matching rows, not zero. The planner now vetoes the endpoint
/// projection while entity-level label use remains downstream.
#[test]
fn parsed_is_labeled_residual_with_projected_far_endpoint_returns_matching_rows() {
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["Gap004A"], Vec::<(&str, Value)>::new())
        .expect("a");
    for (label, name) in [("Gap004Y", "hit"), ("Gap004Z", "skip")] {
        let b = store
            .insert_vertex_named([label], [("name", Value::Text(name.into()))])
            .expect("b vertex");
        store
            .insert_directed_edge_named(a, b, Some("Gap004Rel"), [("weight", Value::Int64(5))])
            .expect("weighted edge");
    }

    // Property-level RETURN: without the fix, IsLabeled(b:Gap004Y) evaluated
    // against a projected Record dropped every row (0 rows instead of 1).
    let projected = plan_with_edge_inlist_stats(
        "MATCH (a:Gap004A)-[e:Gap004Rel WHERE e.weight = 5]->(b:Gap004Y) RETURN b.name",
    );
    let result = store
        .execute_plan_query(&projected, &params(), GqlExecutionContext::default())
        .expect("execute projected far endpoint query");
    assert_eq!(
        text_column(&result, "b.name"),
        vec!["hit".to_string()],
        "IsLabeled(b) must filter to the Gap004Y endpoint, not drop all rows"
    );

    // Whole-variable twin: same shape, `RETURN b` keeps full hydration either way.
    let whole = plan_with_edge_inlist_stats(
        "MATCH (a:Gap004A)-[e:Gap004Rel WHERE e.weight = 5]->(b:Gap004Y) RETURN b",
    );
    let result = store
        .execute_plan_query(&whole, &params(), GqlExecutionContext::default())
        .expect("execute whole-variable far endpoint query");
    assert_eq!(
        result.rows.len(),
        1,
        "whole-variable RETURN b must bind exactly the Gap004Y endpoint"
    );
}

/// GAP-2026-08-24-003 regression: in a combined cypher+sql-compat build the
/// parsed bracket form `IN [5, 7]` must execute and return the matching rows.
/// The sql-compat arm used to claim `IN` unconditionally and require `(`, so
/// this query failed to parse before it ever reached the executor.
#[test]
#[cfg(all(feature = "cypher", feature = "sql-compat"))]
fn parsed_bracket_in_list_returns_matching_rows_under_combined_dialects() {
    let store = GraphStore::new();
    for (name, age) in [("five", 5), ("six", 6), ("seven", 7)] {
        store
            .insert_vertex_named(
                ["Gap003P"],
                [
                    ("name", Value::Text(name.into())),
                    ("age", Value::Int64(age)),
                ],
            )
            .expect("vertex");
    }

    let run = |input: &str| {
        let plan = plan_with_edge_inlist_stats(input);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .unwrap_or_else(|err| panic!("execute {input}: {err:?}"));
        let mut names = text_column(&result, "p.name");
        names.sort();
        names
    };

    assert_eq!(
        run("MATCH (p:Gap003P) WHERE p.age IN [5, 7] RETURN p.name"),
        vec!["five".to_string(), "seven".to_string()],
        "bracket-form IN must filter to ages 5 and 7"
    );
    let paren = run("MATCH (p:Gap003P) WHERE p.age IN (5, 7) RETURN p.name");
    assert_eq!(paren, vec!["five".to_string(), "seven".to_string()]);
}

#[test]
fn indexed_edge_inlist_parameter_elements_bind_same_rows_as_literals() {
    let store = GraphStore::new();
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let a = store
        .insert_vertex_named(["EdgeInParamA"], Vec::<(&str, Value)>::new())
        .expect("a");
    for (name, weight) in [("w5", 5i64), ("w7", 7), ("w9", 9)] {
        let b = store
            .insert_vertex_named(["EdgeInParamB"], [("name", Value::Text(name.into()))])
            .expect("b vertex");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("EdgeInParamRel"),
                [("weight", Value::Int64(weight))],
            )
            .expect("weighted edge");
    }

    // The parsed plan keeps both parameter elements as independently resolved
    // union probes.
    let query = "MATCH (a:EdgeInParamA)-[e:EdgeInParamRel]->(b:EdgeInParamB) \
                 WHERE e.weight IN [$lo, $hi] RETURN b";
    let parameterized = plan_with_edge_inlist_stats(query);
    assert!(
        parameterized.ops.iter().any(|op| matches!(
            op,
            PlanOp::EdgeIndexScan {
                value: ScanValue::InList(elements),
                cmp: CmpOp::Eq,
                ..
            } if *elements
                == vec![
                    ScanValue::Parameter("$lo".into()),
                    ScanValue::Parameter("$hi".into()),
                ]
        )),
        "expected anchored EdgeIndexScan with per-element parameter probes for {query}, got {:?}",
        parameterized.ops
    );

    // Executor parameter maps are keyed by bare names (`param_map_key` strips
    // the `$` sigil), matching the Router wire convention.
    let mut params_map = std::collections::BTreeMap::new();
    params_map.insert("lo".to_string(), Value::Int64(5));
    params_map.insert("hi".to_string(), Value::Int64(7));

    let literal_plan = plan_with_edge_inlist_stats(
        "MATCH (a:EdgeInParamA)-[e:EdgeInParamRel]->(b:EdgeInParamB) \
         WHERE e.weight IN [5, 7] RETURN b",
    );

    let bound_b = |plan: &PhysicalPlan, params: &std::collections::BTreeMap<String, Value>| {
        let result = store
            .execute_plan_query(plan, params, GqlExecutionContext::default())
            .unwrap_or_else(|err| panic!("execute edge IN scan with params {params:?}: {err:?}"));
        let mut rows: Vec<String> = result
            .rows
            .iter()
            .map(|row| format!("{:?}", row.get("b")))
            .collect();
        rows.sort();
        rows
    };

    // Parameter elements must resolve to exactly the rows their inline literals bind.
    assert_eq!(
        bound_b(&parameterized, &params_map).len(),
        2,
        "weights 5 and 7 each bind one row"
    );
    assert_eq!(
        bound_b(&parameterized, &params_map),
        bound_b(&literal_plan, &params_map),
        "parameterized and literal IN lists must bind identical row sets"
    );
}

#[test]
fn empty_inlist_binds_zero_rows_for_vertex_and_edge_index_scans() {
    // `IN []` lowers into an anchored union of nothing on both sides: no probe
    // can fire, so the index path contributes zero rows - the same semantics the
    // residual-filtered predicate would produce without any index.
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");

    // Edge side: three candidate edges exist, but the empty union binds none.
    let (store, _) = edge_inlist_store();
    let edge_plan = edge_inlist_scan_plan("EdgeInRel", Vec::new(), CmpOp::Eq);
    let edge_result = store
        .execute_plan_query(&edge_plan, &params(), GqlExecutionContext::default())
        .expect("execute empty edge in-list scan");
    assert!(
        edge_result.rows.is_empty(),
        "an empty IN list must contribute no rows"
    );

    // Vertex side: the identical union-of-nothing contract over the mock index.
    let vertex_store = GraphStore::new();
    configure_test_index(&vertex_store);
    let (index, vertex_plan, _, _, _, _, _) = inlist_scan_fixture(&vertex_store, Vec::new());
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("uid"),
    ]);
    let vertex_result = pollster::block_on(execute_plan_query(
        &vertex_store,
        &vertex_plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute empty vertex in-list scan");
    assert!(
        vertex_result.rows.is_empty(),
        "an empty IN list must contribute no rows"
    );
    assert!(
        index.equal_calls.borrow().is_empty(),
        "an empty probe union must issue no equality lookups"
    );
}

#[test]
fn range_index_scan_returns_no_rows_for_empty_clamped_interval() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(["IndexScanEmptyRange"], [("at", Value::DateTime(0, 0))])
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "at".into(),
        value: ScanValue::Literal(Value::DateTime(i64::MAX, u32::MAX)),
        cmp: CmpOp::Gt,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("at"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute empty-clamped range scan");

    // Nothing is strictly greater than the domain maximum: no lookup is issued at all.
    assert!(result.rows.is_empty());
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn range_index_scan_falls_back_to_filter_path_for_unsupported_domains() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanUnsupportedRange"],
            [
                ("tags", Value::List(vec![Value::Int64(2)])),
                ("name", Value::Text("hit".into())),
            ],
        )
        .expect("insert hit");
    store
        .insert_vertex_named(
            ["IndexScanUnsupportedRange"],
            [
                ("tags", Value::List(vec![Value::Int64(0)])),
                ("name", Value::Text("miss".into())),
            ],
        )
        .expect("insert miss");
    let index = MockPropertyIndex::default();
    let bound = Value::List(vec![Value::Int64(1)]);
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Ge,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "tags")),
                op: CmpOp::Ge,
                right: Box::new(Expr::new(ExprKind::Literal(bound))),
            })],
            stage: 0,
        },
        PlanOp::Project {
            columns: vec![project(prop("n", "name"), "n.name")],
            distinct: false,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("tags"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute list-domain range fallback");

    // List has no contiguous ordered comparison domain: no pushdown; the residual filter
    // answers over the node scan.
    assert!(index.range_calls.borrow().is_empty());
    assert!(index.equal_calls.borrow().is_empty());
    assert_eq!(text_column(&result, "n.name"), vec!["hit"]);
}

#[test]
fn range_index_scan_falls_back_to_filter_path_for_list_domain() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanListRange"],
            [
                ("tags", Value::List(vec![Value::Int64(2)])),
                ("name", Value::Text("hit".into())),
            ],
        )
        .expect("insert hit");
    store
        .insert_vertex_named(
            ["IndexScanListRange"],
            [
                ("tags", Value::List(vec![Value::Int64(0)])),
                ("name", Value::Text("miss".into())),
            ],
        )
        .expect("insert miss");
    let index = MockPropertyIndex::default();
    let bound = Value::List(vec![Value::Int64(1)]);
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Ge,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "tags")),
                op: CmpOp::Ge,
                right: Box::new(Expr::new(ExprKind::Literal(bound))),
            })],
            stage: 0,
        },
        PlanOp::Project {
            columns: vec![project(prop("n", "name"), "n.name")],
            distinct: false,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("tags"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute list range fallback");

    assert!(index.range_calls.borrow().is_empty());
    assert!(index.equal_calls.borrow().is_empty());
    assert_eq!(text_column(&result, "n.name"), vec!["hit"]);
}

#[test]
fn range_index_scan_falls_back_to_filter_path_for_record_domain() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanRecordRange"],
            [
                (
                    "profile",
                    Value::Record(vec![
                        ("a".into(), Value::Int64(1)),
                        ("b".into(), Value::Int64(1)),
                    ]),
                ),
                ("name", Value::Text("hit".into())),
            ],
        )
        .expect("insert hit");
    store
        .insert_vertex_named(
            ["IndexScanRecordRange"],
            [
                (
                    "profile",
                    Value::Record(vec![
                        ("a".into(), Value::Int64(5)),
                        ("b".into(), Value::Int64(5)),
                    ]),
                ),
                ("name", Value::Text("miss".into())),
            ],
        )
        .expect("insert miss");
    let index = MockPropertyIndex::default();
    let bound = Value::Record(vec![
        ("b".into(), Value::Int64(3)),
        ("a".into(), Value::Int64(3)),
    ]);
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "profile".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Lt,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "profile")),
                op: CmpOp::Lt,
                right: Box::new(Expr::new(ExprKind::Literal(bound))),
            })],
            stage: 0,
        },
        PlanOp::Project {
            columns: vec![project(prop("n", "name"), "n.name")],
            distinct: false,
        },
    ]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("profile"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute record range fallback");

    assert!(index.range_calls.borrow().is_empty());
    assert!(index.equal_calls.borrow().is_empty());
    assert_eq!(text_column(&result, "n.name"), vec!["hit"]);
}

#[test]
fn range_index_scan_does_not_push_down_extension_domain() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanExtensionRange"],
            [("principal", orderable_ext(7))],
        )
        .expect("insert hit");
    store
        .insert_vertex_named(
            ["IndexScanExtensionRange"],
            [("principal", orderable_ext(5))],
        )
        .expect("insert miss");
    let index = MockPropertyIndex::default();
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "principal".into(),
        value: ScanValue::Literal(orderable_ext(7)),
        cmp: CmpOp::Ge,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("principal"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute extension range without pushdown");

    // Extension has no contiguous ordered comparison domain, so nothing is pushed down:
    // the answer comes from the non-index path. (Exact residual filtering of extension
    // comparisons is owned by the expression evaluator / extension codecs, not this gate.)
    assert!(index.range_calls.borrow().is_empty());
    assert!(index.equal_calls.borrow().is_empty());
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn executes_orderable_extension_equality_index_scan() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let value = orderable_ext(7);
    store
        .insert_vertex_named(
            ["IndexScanExtensionEqCatalog"],
            [("principal", Value::Text("catalog".into()))],
        )
        .expect("insert catalog vertex");
    let vid = store
        .insert_vertex_named(["IndexScanExtensionEq"], Vec::<(&str, Value)>::new())
        .expect("insert vertex");
    let pid = crate::test_labels::property_id_for_name("principal").raw();
    let index = MockPropertyIndex::default();
    index.equal_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "principal".into(),
        value: ScanValue::Literal(value.clone()),
        cmp: CmpOp::Eq,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("principal"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute extension equality index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.equal_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert_eq!(
        calls[0].1,
        value_to_index_key_bytes(&value).unwrap().unwrap()
    );
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn index_scan_rejects_unsupported_parameter_value() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(["IndexScanBadParam"], [("tags", Value::List(vec![]))])
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "tags".into(),
        value: ScanValue::Parameter("tags".into()),
        cmp: CmpOp::Eq,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("unsupported parameter should fail");

    assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
}

#[test]
fn index_scan_rejects_oversized_index_key_before_index_call() {
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    let store = GraphStore::new();
    configure_test_index(&store);
    let index = MockPropertyIndex::default();
    let oversized = Value::Bytes(vec![1u8; MAX_INDEX_VALUE_KEY_BYTES - 2]);
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "country".into(),
        value: ScanValue::Literal(oversized),
        cmp: CmpOp::Eq,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("oversized index key should fail before index call");

    assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    assert!(index.equal_calls.borrow().is_empty());
}

#[test]
fn range_index_scan_rejects_oversized_bound_before_index_call() {
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    let store = GraphStore::new();
    configure_test_index(&store);
    let index = MockPropertyIndex::default();
    let oversized = Value::Bytes(vec![1u8; MAX_INDEX_VALUE_KEY_BYTES - 2]);
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "country".into(),
        value: ScanValue::Literal(oversized),
        cmp: CmpOp::Ge,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("oversized range bound should fail before index call");

    assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn index_scan_rejects_non_orderable_extension_parameter_value() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanBadExtensionParam"],
            [("principal", Value::Text("catalog".into()))],
        )
        .expect("insert catalog vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("principal".into(), non_orderable_ext());
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "principal".into(),
        value: ScanValue::Parameter("principal".into()),
        cmp: CmpOp::Eq,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("non-orderable extension parameter should fail");

    assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
}

#[test]
fn range_index_scan_rejects_unsupported_nested_parameter_value() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(["IndexScanBadRangeParam"], [("tags", Value::List(vec![]))])
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "tags".into(),
        value: ScanValue::Parameter("tags".into()),
        cmp: CmpOp::Ge,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("unsupported range parameter should fail");

    assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
}

#[test]
fn index_scan_rejects_non_finite_float_parameter_value() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(["IndexScanBadFloatParam"], [("score", Value::Float64(1.0))])
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("score".into(), Value::Float64(f64::INFINITY));
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "score".into(),
        value: ScanValue::Parameter("score".into()),
        cmp: CmpOp::Eq,
        property_projection: None,
        ordered_by_sort: None,
    }]);

    let err = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect_err("non-finite parameter should fail");

    assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
}

#[test]
fn conditional_index_scan_falls_back_for_null_or_unsupported_parameter() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanConditionalFallback"],
            [("tags", Value::List(vec![]))],
        )
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
    let plan = plan(vec![PlanOp::ConditionalIndexScan {
        candidates: vec![ConditionalScanCandidate {
            param_name: "tags".into(),
            property: "tags".into(),
            variable: "n".into(),
            cmp: CmpOp::Eq,
        }],
        fallback_label: Some("IndexScanConditionalFallback".into()),
        fallback_variable: "n".into(),
        property_projection: None,
    }]);

    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("conditional fallback");

    assert_eq!(result.rows.len(), 1);
    assert!(index.equal_calls.borrow().is_empty());
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn conditional_index_scan_falls_back_for_non_orderable_extension_parameter() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanConditionalExtensionFallback"],
            Vec::<(&str, Value)>::new(),
        )
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("principal".into(), non_orderable_ext());
    let plan = plan(vec![PlanOp::ConditionalIndexScan {
        candidates: vec![ConditionalScanCandidate {
            param_name: "principal".into(),
            property: "principal".into(),
            variable: "n".into(),
            cmp: CmpOp::Eq,
        }],
        fallback_label: Some("IndexScanConditionalExtensionFallback".into()),
        fallback_variable: "n".into(),
        property_projection: None,
    }]);

    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("conditional fallback");

    assert_eq!(result.rows.len(), 1);
    assert!(index.equal_calls.borrow().is_empty());
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn conditional_range_index_scan_falls_back_for_unsupported_nested_parameter() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanConditionalRangeFallback"],
            [("tags", Value::List(vec![]))],
        )
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
    let plan = plan(vec![PlanOp::ConditionalIndexScan {
        candidates: vec![ConditionalScanCandidate {
            param_name: "tags".into(),
            property: "tags".into(),
            variable: "n".into(),
            cmp: CmpOp::Ge,
        }],
        fallback_label: Some("IndexScanConditionalRangeFallback".into()),
        fallback_variable: "n".into(),
        property_projection: None,
    }]);

    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("conditional fallback");

    assert_eq!(result.rows.len(), 1);
    assert!(index.equal_calls.borrow().is_empty());
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn conditional_index_scan_falls_back_for_non_finite_float_parameter() {
    let store = GraphStore::new();
    configure_test_index(&store);
    store
        .insert_vertex_named(
            ["IndexScanConditionalFloatFallback"],
            [("score", Value::Float64(1.0))],
        )
        .expect("insert vertex");
    let index = MockPropertyIndex::default();
    let mut parameters = params();
    parameters.insert("score".into(), Value::Float64(f64::NAN));
    let plan = plan(vec![PlanOp::ConditionalIndexScan {
        candidates: vec![ConditionalScanCandidate {
            param_name: "score".into(),
            property: "score".into(),
            variable: "n".into(),
            cmp: CmpOp::Eq,
        }],
        fallback_label: Some("IndexScanConditionalFloatFallback".into()),
        fallback_variable: "n".into(),
        property_projection: None,
    }]);

    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &parameters,
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("conditional fallback");

    assert_eq!(result.rows.len(), 1);
    assert!(index.equal_calls.borrow().is_empty());
    assert!(index.range_calls.borrow().is_empty());
}

#[test]
fn planner_limit_stops_node_scan_after_enough_rows() {
    let store = GraphStore::new();
    store
        .insert_vertex_named(
            ["PlannerQueryLazyLimit"],
            [("name", Value::Text("first".into()))],
        )
        .expect("insert first");
    for i in 0..64 {
        store
            .insert_vertex_named(
                ["PlannerQueryLazyLimit"],
                [("name", Value::Text(format!("tail {i}")))],
            )
            .expect("insert tail");
    }
    let plan = plan_gql("MATCH (n:PlannerQueryLazyLimit) RETURN n.name LIMIT 1");

    reset_node_scan_visits();
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute planned query");

    assert_eq!(text_column(&result, "n.name"), vec!["first"]);
    assert_eq!(node_scan_visits(), 1);
}

#[test]
fn planner_limit_stops_after_filter_accepts_enough_rows() {
    let store = GraphStore::new();
    for i in 0..10 {
        store
            .insert_vertex_named(
                ["PlannerQueryLazyFilterLimit"],
                [
                    ("name", Value::Text(format!("drop {i}"))),
                    ("keep", Value::Bool(false)),
                ],
            )
            .expect("insert dropped");
    }
    for name in ["keep a", "keep b"] {
        store
            .insert_vertex_named(
                ["PlannerQueryLazyFilterLimit"],
                [
                    ("name", Value::Text(name.into())),
                    ("keep", Value::Bool(true)),
                ],
            )
            .expect("insert kept");
    }
    for i in 0..32 {
        store
            .insert_vertex_named(
                ["PlannerQueryLazyFilterLimit"],
                [
                    ("name", Value::Text(format!("unvisited {i}"))),
                    ("keep", Value::Bool(true)),
                ],
            )
            .expect("insert tail");
    }
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("PlannerQueryLazyFilterLimit".into()),
            property_projection: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "keep")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
            })],
            stage: 0,
        },
        PlanOp::Limit {
            count: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
            offset: None,
        },
        PlanOp::Project {
            columns: vec![project(prop("n", "name"), "n.name")],
            distinct: false,
        },
    ]);

    reset_node_scan_visits();
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute planned query");

    assert_eq!(text_column(&result, "n.name"), vec!["keep a", "keep b"]);
    assert_eq!(node_scan_visits(), 12);
}

#[test]
fn order_by_limit_remains_a_materializing_barrier() {
    let store = GraphStore::new();
    for name in ["c", "a", "b"] {
        store
            .insert_vertex_named(
                ["PlannerQueryLazyLimitSort"],
                [("name", Value::Text(name.into()))],
            )
            .expect("insert vertex");
    }
    let plan =
        plan_gql("MATCH (n:PlannerQueryLazyLimitSort) RETURN n.name ORDER BY n.name LIMIT 1");

    reset_node_scan_visits();
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute planned query");

    assert_eq!(text_column(&result, "n.name"), vec!["a"]);
    assert_eq!(node_scan_visits(), 3);
}

#[test]
fn labeled_expand_limit_offset_pages_earliest_edges() {
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["LazyEdgePageSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    for i in 0..5 {
        let dst = store
            .insert_vertex_named(
                ["LazyEdgePageTarget"],
                [("name", Value::Text(format!("edge {i}")))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(
                src,
                dst,
                Some("LazyEdgePageRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");
    }

    let first_page = plan_gql(
        "MATCH (a:LazyEdgePageSource)-[:LazyEdgePageRel]->(b) RETURN b.name LIMIT 2 OFFSET 0",
    );
    let second_page = plan_gql(
        "MATCH (a:LazyEdgePageSource)-[:LazyEdgePageRel]->(b) RETURN b.name LIMIT 2 OFFSET 2",
    );

    reset_edge_stream_visits();
    let first = store
        .execute_plan_query(&first_page, &params(), GqlExecutionContext::default())
        .expect("execute first page");

    reset_edge_stream_visits();
    let second = store
        .execute_plan_query(&second_page, &params(), GqlExecutionContext::default())
        .expect("execute second page");

    assert_eq!(text_column(&first, "b.name"), vec!["edge 0", "edge 1"]);
    assert_eq!(text_column(&second, "b.name"), vec!["edge 2", "edge 3"]);
}

#[test]
fn insertion_order_asc_pages_labeled_edges_in_insertion_order() {
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["SeqAscPageSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    for i in 0..5 {
        let dst = store
            .insert_vertex_named(
                ["SeqAscPageTarget"],
                [("name", Value::Text(format!("seq edge {i}")))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(
                src,
                dst,
                Some("SeqAscPageRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");
    }

    let page = plan_gql(
        "MATCH (a:SeqAscPageSource)-[e:SeqAscPageRel]->(b) \
             ORDER BY INSERTION(e) ASC LIMIT 2 OFFSET 1 RETURN b.name",
    );

    let result = store
        .execute_plan_query(
            &page,
            &params(),
            resolved_execution_ctx(
                &["SeqAscPageSource", "SeqAscPageTarget"],
                &[("SeqAscPageRel", EdgeOrderingPolicy::Insertion)],
            ),
        )
        .expect("execute asc page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["seq edge 1", "seq edge 2"]
    );
}

#[test]
fn insertion_order_after_intermediate_unnamed_expand() {
    // Regression for social-demo-style queries: the edge variable used in
    // `ORDER BY INSERTION(e)` is bound by an earlier expand, while a later
    // unnamed expand walks a different relationship. The sort must skip the
    // intermediate expand and still recognize that `e` is bound.
    let store = GraphStore::new();
    let feed = store
        .insert_vertex_named(["SeqFeed"], Vec::<(&str, Value)>::new())
        .expect("insert feed");
    let mut posts = Vec::new();
    let mut authors = Vec::new();
    for i in 0..3 {
        let post = store
            .insert_vertex_named(
                ["SeqFeedPost"],
                [("title", Value::Text(format!("post {i}")))],
            )
            .expect("insert post");
        let author = store
            .insert_vertex_named(
                ["SeqFeedAuthor"],
                [("name", Value::Text(format!("author {i}")))],
            )
            .expect("insert author");
        store
            .insert_directed_edge_named(post, feed, Some("IN_FEED"), Vec::<(&str, Value)>::new())
            .expect("insert feed edge");
        store
            .insert_directed_edge_named(author, post, Some("POSTED"), Vec::<(&str, Value)>::new())
            .expect("insert posted edge");
        posts.push(post);
        authors.push(author);
    }

    let plan = plan_gql(
        "MATCH (feed:SeqFeed)<-[e:IN_FEED]-(p:SeqFeedPost)<-[:POSTED]-(author:SeqFeedAuthor) \
             RETURN author.name AS author_name ORDER BY INSERTION(e) DESC LIMIT 2",
    );

    let result = store
        .execute_plan_query(
            &plan,
            &params(),
            resolved_execution_ctx(
                &["SeqFeed", "SeqFeedPost", "SeqFeedAuthor"],
                &[
                    ("IN_FEED", EdgeOrderingPolicy::Insertion),
                    ("POSTED", EdgeOrderingPolicy::Unordered),
                ],
            ),
        )
        .expect("execute sequence order after intermediate expand");

    assert_eq!(
        text_column(&result, "author_name"),
        vec!["author 2", "author 1"]
    );
}

#[test]
fn insertion_order_survives_non_rebinding_optional_match() {
    // Regression for the social-demo reply tree: `e` is bound by the materialized feed edge,
    // then an OPTIONAL MATCH may bind a different reply edge before the final insertion order.
    let store = GraphStore::new();
    let feed = store
        .insert_vertex_named(["SeqOptionalFeed"], Vec::<(&str, Value)>::new())
        .expect("insert feed");
    let parent = store
        .insert_vertex_named(
            ["SeqOptionalPost"],
            [("name", Value::Text("parent".to_owned()))],
        )
        .expect("insert parent");
    let first = store
        .insert_vertex_named(
            ["SeqOptionalPost"],
            [("name", Value::Text("first".to_owned()))],
        )
        .expect("insert first post");
    let second = store
        .insert_vertex_named(
            ["SeqOptionalPost"],
            [("name", Value::Text("second".to_owned()))],
        )
        .expect("insert second post");
    store
        .insert_directed_edge_named(first, feed, Some("IN_FEED"), Vec::<(&str, Value)>::new())
        .expect("insert first feed edge");
    store
        .insert_directed_edge_named(second, feed, Some("IN_FEED"), Vec::<(&str, Value)>::new())
        .expect("insert second feed edge");
    store
        .insert_directed_edge_named(
            second,
            parent,
            Some("REPLY_TO"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("insert reply edge");

    let plan = plan_gql(
        "MATCH (feed:SeqOptionalFeed)<-[e:IN_FEED]-(p:SeqOptionalPost) \
         OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:SeqOptionalPost) \
         RETURN p.name AS name, parent.name AS parent_name \
         ORDER BY INSERTION(e) DESC LIMIT 2",
    );

    let result = store
        .execute_plan_query(
            &plan,
            &params(),
            resolved_execution_ctx(
                &["SeqOptionalFeed", "SeqOptionalPost"],
                &[
                    ("IN_FEED", EdgeOrderingPolicy::Insertion),
                    ("REPLY_TO", EdgeOrderingPolicy::Unordered),
                ],
            ),
        )
        .expect("execute sequence order after optional reply match");

    assert_eq!(text_column(&result, "name"), vec!["second", "first"]);
    assert_eq!(
        result.rows[0].get("parent_name"),
        Some(&Value::Text("parent".to_owned()))
    );
    assert_eq!(result.rows[1].get("parent_name"), Some(&Value::Null));
}

#[test]
fn optional_match_edge_binding_detection_preserves_rebinding_boundary() {
    let rebind = plan_gql("MATCH (a:SeqOptionalRebindA)-[e:SeqOptionalRebindRel]->(b) RETURN e");
    let different_edge =
        plan_gql("MATCH (a:SeqOptionalRebindA)-[r:SeqOptionalRebindRel]->(b) RETURN r");

    assert!(super::super::ops_bind_edge_variable(
        &[PlanOp::OptionalMatch {
            sub_plan: rebind.ops,
        }],
        "e",
    ));
    assert!(!super::super::ops_bind_edge_variable(
        &[PlanOp::OptionalMatch {
            sub_plan: different_edge.ops,
        }],
        "e",
    ));
}

#[test]
fn previous_op_binds_edge_uses_most_recent_binding_for_rebound_variable() {
    // If the same edge variable is bound by a later expand, the insertion order
    // must apply to the later binding, not the earlier one.
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["SeqRebindA"], Vec::<(&str, Value)>::new())
        .expect("insert a");
    let b = store
        .insert_vertex_named(["SeqRebindB"], Vec::<(&str, Value)>::new())
        .expect("insert b");
    let c = store
        .insert_vertex_named(["SeqRebindC"], Vec::<(&str, Value)>::new())
        .expect("insert c");
    let d = store
        .insert_vertex_named(["SeqRebindD"], [("name", Value::Text("d".to_owned()))])
        .expect("insert d");

    // a-[e:First]->b, b-[e:Second]->c, c-[f:Third]->d
    store
        .insert_directed_edge_named(a, b, Some("First"), Vec::<(&str, Value)>::new())
        .expect("insert first edge");
    store
        .insert_directed_edge_named(b, c, Some("Second"), Vec::<(&str, Value)>::new())
        .expect("insert second edge");
    store
        .insert_directed_edge_named(c, d, Some("Third"), Vec::<(&str, Value)>::new())
        .expect("insert third edge");

    let plan = plan_gql(
        "MATCH (a:SeqRebindA)-[e:First]->(b:SeqRebindB)-[e:Second]->(c:SeqRebindC)-[f:Third]->(d:SeqRebindD) \
             RETURN d.name AS name ORDER BY INSERTION(e) DESC LIMIT 1",
    );

    let result = store
        .execute_plan_query(
            &plan,
            &params(),
            resolved_execution_ctx(
                &["SeqRebindA", "SeqRebindB", "SeqRebindC", "SeqRebindD"],
                &[
                    ("First", EdgeOrderingPolicy::Insertion),
                    ("Second", EdgeOrderingPolicy::Insertion),
                    ("Third", EdgeOrderingPolicy::Unordered),
                ],
            ),
        )
        .expect("execute rebind sequence order");

    // There is only one path, and e is bound to the single Second edge.
    assert_eq!(text_column(&result, "name"), vec!["d"]);
}

#[test]
fn insertion_order_desc_returns_newest_edges_first() {
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["SeqDescPageSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    for i in 0..4 {
        let dst = store
            .insert_vertex_named(
                ["SeqDescPageTarget"],
                [("name", Value::Text(format!("seq desc edge {i}")))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(
                src,
                dst,
                Some("SeqDescPageRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");
    }

    let page = plan_gql(
        "MATCH (a:SeqDescPageSource)-[e:SeqDescPageRel]->(b) \
             ORDER BY INSERTION(e) DESC LIMIT 2 RETURN b.name",
    );

    let result = store
        .execute_plan_query(
            &page,
            &params(),
            resolved_execution_ctx(
                &["SeqDescPageSource", "SeqDescPageTarget"],
                &[("SeqDescPageRel", EdgeOrderingPolicy::Insertion)],
            ),
        )
        .expect("execute desc page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["seq desc edge 3", "seq desc edge 2"]
    );
}

#[test]
fn insertion_order_rejects_unlabeled_edge_pattern() {
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["SeqNoLabelSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    let dst = store
        .insert_vertex_named(["SeqNoLabelTarget"], Vec::<(&str, Value)>::new())
        .expect("insert target");
    store
        .insert_directed_edge_named(src, dst, Option::<&str>::None, Vec::<(&str, Value)>::new())
        .expect("insert edge");

    let page = plan_gql(
        "MATCH (a:SeqNoLabelSource)-[e]->(b) \
             ORDER BY INSERTION(e) ASC RETURN b",
    );

    let err = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect_err("unlabeled insertion order should fail");

    assert!(err.to_string().contains("single fixed edge label"), "{err}");
}

#[test]
fn insertion_order_rejects_unordered_label() {
    // ADR 0052 §3: the capability must be declared in the Graph Type. A fixed label whose
    // resolved policy is Unordered fails closed instead of silently using physical slot order.
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["SeqUnorderedSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    let dst = store
        .insert_vertex_named(["SeqUnorderedTarget"], Vec::<(&str, Value)>::new())
        .expect("insert target");
    store
        .insert_directed_edge_named(
            src,
            dst,
            Some("SeqUnorderedRel"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("insert edge");

    let page = plan_gql(
        "MATCH (a:SeqUnorderedSource)-[e:SeqUnorderedRel]->(b) \
             ORDER BY INSERTION(e) ASC RETURN b",
    );

    let unordered_ctx = resolved_execution_ctx(
        &["SeqUnorderedSource", "SeqUnorderedTarget"],
        &[("SeqUnorderedRel", EdgeOrderingPolicy::Unordered)],
    );
    let err = store
        .execute_plan_query(&page, &params(), unordered_ctx)
        .expect_err("unordered label must fail closed");

    assert!(err.to_string().contains("ORDER BY INSERTION"), "{err}");
    assert!(err.to_string().contains("SeqUnorderedRel"), "{err}");
}

#[test]
fn insertion_order_rejects_label_missing_from_resolved_table() {
    // A label that is not in the Router-resolved label table has no declared policy and must
    // fail closed rather than being invented from a test registry.
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["SeqMissingSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    let dst = store
        .insert_vertex_named(["SeqMissingTarget"], Vec::<(&str, Value)>::new())
        .expect("insert target");
    store
        .insert_directed_edge_named(src, dst, Some("SeqMissingRel"), Vec::<(&str, Value)>::new())
        .expect("insert edge");

    let page = plan_gql(
        "MATCH (a:SeqMissingSource)-[e:SeqMissingRel]->(b) \
             ORDER BY INSERTION(e) ASC RETURN b",
    );

    // The table projects the vertex labels but not the edge label, mirroring a Router that
    // resolved no policy for it.
    let missing_ctx = resolved_execution_ctx(&["SeqMissingSource", "SeqMissingTarget"], &[]);
    let err = store
        .execute_plan_query(&page, &params(), missing_ctx)
        .expect_err("label absent from resolved table must fail closed");

    assert!(err.to_string().contains("ORDER BY INSERTION"), "{err}");
    assert!(err.to_string().contains("SeqMissingRel"), "{err}");
}

#[test]
fn unlabeled_directed_expand_limit_offset_uses_earliest_edges() {
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["LazyUnlabeledPageSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    for i in 0..5 {
        let dst = store
            .insert_vertex_named(
                ["LazyUnlabeledPageTarget"],
                [("name", Value::Text(format!("unlabeled edge {i}")))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(src, dst, Option::<&str>::None, Vec::<(&str, Value)>::new())
            .expect("insert edge");
    }

    let page = plan_gql("MATCH (a:LazyUnlabeledPageSource)-[]->(b) RETURN b.name LIMIT 2 OFFSET 2");

    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["unlabeled edge 2", "unlabeled edge 3"]
    );
}

#[test]
fn reverse_expand_limit_offset_uses_earliest_in_edges() {
    let store = GraphStore::new();
    let dst = store
        .insert_vertex_named(["LazyReversePageTarget"], Vec::<(&str, Value)>::new())
        .expect("insert target");
    for i in 0..5 {
        let src = store
            .insert_vertex_named(
                ["LazyReversePageSource"],
                [("name", Value::Text(format!("reverse edge {i}")))],
            )
            .expect("insert source");
        store
            .insert_directed_edge_named(
                src,
                dst,
                Some("LazyReversePageRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");
    }

    let page = plan_gql(
        "MATCH (b:LazyReversePageTarget)<-[:LazyReversePageRel]-(a) RETURN a.name LIMIT 2 OFFSET 2",
    );

    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "a.name"),
        vec!["reverse edge 2", "reverse edge 3"]
    );
}

#[test]
fn undirected_expand_limit_offset_uses_earliest_edges() {
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["LazyUndirectedPageSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    for i in 0..5 {
        let dst = store
            .insert_vertex_named(
                ["LazyUndirectedPageTarget"],
                [("name", Value::Text(format!("undirected edge {i}")))],
            )
            .expect("insert target");
        store
            .insert_undirected_edge_named(
                src,
                dst,
                Option::<&str>::None,
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");
    }

    let page = plan_gql("MATCH (a:LazyUndirectedPageSource)~[]~(b) RETURN b.name LIMIT 2 OFFSET 2");

    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["undirected edge 2", "undirected edge 3"]
    );
}

#[test]
fn filtered_expand_limit_offset_skips_only_matching_edges() {
    let store = GraphStore::new();
    let src = store
        .insert_vertex_named(["LazyFilteredPageSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    for (i, keep) in [
        (0, true),
        (1, false),
        (2, true),
        (3, false),
        (4, true),
        (5, true),
    ] {
        let dst = store
            .insert_vertex_named(
                ["LazyFilteredPageTarget"],
                [
                    ("name", Value::Text(format!("filtered edge {i}"))),
                    ("keep", Value::Bool(keep)),
                ],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(
                src,
                dst,
                Some("LazyFilteredPageRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");
    }

    let page = plan_gql(
        "MATCH (a:LazyFilteredPageSource)-[:LazyFilteredPageRel]->(b) \
             WHERE b.keep = true RETURN b.name LIMIT 2 OFFSET 1",
    );

    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["filtered edge 2", "filtered edge 4"]
    );
}

#[test]
fn node_scan_projects_vertex_property() {
    let store = GraphStore::new();
    store
        .insert_vertex_named(
            ["QueryPersonNodeScan"],
            [("name", Value::Text("Node Alice".into()))],
        )
        .expect("insert vertex");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("QueryPersonNodeScan".into()),
            property_projection: None,
        },
        PlanOp::Project {
            columns: vec![project(prop("n", "name"), "name")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("name"),
        Some(&Value::Text("Node Alice".into()))
    );
}

#[test]
fn indexed_expand_limit_offset_skips_only_matching_edges() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["IdxEqPageA"], Vec::<(&str, Value)>::new())
        .expect("a");
    for (i, weight) in [(0, 5), (1, 9), (2, 5), (3, 9), (4, 5), (5, 5)] {
        let b = store
            .insert_vertex_named(
                ["IdxEqPageB"],
                [("name", Value::Text(format!("indexed edge {i}")))],
            )
            .expect("b");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("IdxEqPageRel"),
                [("weight", Value::Int64(weight))],
            )
            .expect("edge");
    }
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("IdxEqPageA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("IdxEqPageRel".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: Some(("weight".into(), ScanValue::Literal(Value::Int64(5)))),
            edge_inline_property_predicate: None,
            edge_inline_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(prop("b", "name"), "name")],
            distinct: false,
        },
        PlanOp::Limit {
            count: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
            offset: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("indexed expand");

    assert_eq!(
        text_column(&result, "name"),
        vec!["indexed edge 2", "indexed edge 4"]
    );
}

#[test]
fn aggregate_count_star_after_node_scan() {
    let store = GraphStore::new();
    store
        .insert_vertex_named(["AggScanLbl"], [("x", Value::Int64(1))])
        .expect("v1");
    store
        .insert_vertex_named(["AggScanLbl"], [("x", Value::Int64(2))])
        .expect("v2");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("AggScanLbl".into()),
            property_projection: None,
        },
        PlanOp::Aggregate {
            group_by: Vec::new(),
            aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("cnt"))],
        },
        PlanOp::Project {
            columns: vec![project(agg_count_star(), "cnt")],
            distinct: false,
        },
    ]);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("count");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
}

#[test]
fn leading_edge_index_scan_binds_matching_edges_and_endpoints() {
    let store = GraphStore::new();
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let a = store
        .insert_vertex_named(["LeadIdxA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b_match = store
        .insert_vertex_named(["LeadIdxB"], Vec::<(&str, Value)>::new())
        .expect("b match");
    let b_miss = store
        .insert_vertex_named(["LeadIdxB"], Vec::<(&str, Value)>::new())
        .expect("b miss");
    store
        .insert_directed_edge_named(
            a,
            b_match,
            Some("LeadIdxRel"),
            [("weight", Value::Int64(5))],
        )
        .expect("match edge");
    store
        .insert_directed_edge_named(a, b_miss, Some("LeadIdxRel"), [("weight", Value::Int64(9))])
        .expect("miss edge");

    let plan = plan(vec![
        PlanOp::EdgeIndexScan {
            variable: "e".into(),
            property: "weight".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Eq,
            property_projection: None,
        },
        PlanOp::EdgeBindEndpoints {
            edge: "e".into(),
            near: "__anon_near".into(),
            far: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("LeadIdxRel".into()),
            near_property_projection: None,
            far_property_projection: None,
            hop_aux_binding: None,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("leading edge index scan");

    assert_eq!(result.rows.len(), 1);
}

#[test]
fn leading_edge_bind_endpoints_hop_aux_returns_inline_property_bytes() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
    let a = store
        .insert_vertex_named(["LeadHopA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["LeadHopB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let label_id = crate::test_labels::edge_label_id_for_name("LeadHopRoad");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: EdgeInlinePropertyEncoding::RawU16,
        },
    );
    let weight_prop = crate::test_labels::property_id_for_name("weight");
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let inline_property_bytes = 7u16.to_le_bytes();
    let edge = store
        .insert_directed_edge_with_inline_property_bytes(
            a,
            b,
            Some(label_id),
            &inline_property_bytes,
        )
        .expect("edge");
    store
        .set_edge_property(
            edge.occurrence(LabeledOrientation::Forward),
            weight_prop,
            Value::Int64(7),
        )
        .expect("edge property");

    let plan = plan(vec![
        PlanOp::EdgeIndexScan {
            variable: "e".into(),
            property: "weight".into(),
            value: ScanValue::Literal(Value::Int64(7)),
            cmp: CmpOp::Eq,
            property_projection: None,
        },
        PlanOp::EdgeBindEndpoints {
            edge: "e".into(),
            near: "__anon_near".into(),
            far: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("LeadHopRoad".into()),
            near_property_projection: None,
            far_property_projection: None,
            hop_aux_binding: Some("e__hop_aux".into()),
        },
        PlanOp::Project {
            columns: vec![project(var("e__hop_aux"), "aux")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("leading edge hop_aux");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("aux"),
        Some(&Value::Bytes(inline_property_bytes.to_vec()))
    );
}

#[test]
fn leading_edge_bind_endpoints_honors_prebound_far_vertex() {
    let store = GraphStore::new();
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let a = store
        .insert_vertex_named(["LeadPreA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b_match = store
        .insert_vertex_named(["LeadPreB"], Vec::<(&str, Value)>::new())
        .expect("b match");
    let b_other = store
        .insert_vertex_named(["LeadPreB"], Vec::<(&str, Value)>::new())
        .expect("b other");
    store
        .insert_directed_edge_named(
            a,
            b_match,
            Some("LeadPreRel"),
            [("weight", Value::Int64(3))],
        )
        .expect("match edge");
    store
        .insert_directed_edge_named(
            a,
            b_other,
            Some("LeadPreRel"),
            [("weight", Value::Int64(3))],
        )
        .expect("other edge");

    let plan = plan(vec![
        PlanOp::EdgeIndexScan {
            variable: "e".into(),
            property: "weight".into(),
            value: ScanValue::Literal(Value::Int64(3)),
            cmp: CmpOp::Eq,
            property_projection: None,
        },
        PlanOp::EdgeBindEndpoints {
            edge: "e".into(),
            near: "__anon_near".into(),
            far: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("LeadPreRel".into()),
            near_property_projection: None,
            far_property_projection: None,
            hop_aux_binding: None,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ]);

    let mut seed = PlanRow::new();
    seed.insert("b".to_owned(), PlanBinding::Vertex(b_match));
    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        None,
        GqlExecutionContext::default(),
        vec![seed],
        false,
    ))
    .expect("prebound far");

    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].get("b"), Some(PlanBinding::Vertex(id)) if *id == b_match));
}

#[test]
fn index_intersection_returns_vertices_in_both_postings() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let vid1 = store
        .insert_vertex_named(["Ix1"], [("uid", Value::Text("alice".into()))])
        .expect("vid1");
    let vid2 = store
        .insert_vertex_named(
            ["Ix2"],
            [
                ("uid", Value::Text("alice".into())),
                ("email", Value::Text("alice@example.com".into())),
            ],
        )
        .expect("vid2");
    let vid3 = store
        .insert_vertex_named(
            ["Ix3"],
            [("email", Value::Text("alice@example.com".into()))],
        )
        .expect("vid3");
    let uid_pid = crate::test_labels::property_id_for_name("uid").raw();
    let email_pid = crate::test_labels::property_id_for_name("email").raw();
    let alice_key = value_to_index_key_bytes(&Value::Text("alice".into()))
        .expect("encode uid")
        .expect("sortable uid");
    let email_key = value_to_index_key_bytes(&Value::Text("alice@example.com".into()))
        .expect("encode email")
        .expect("sortable email");
    let local_shard = store.federation_routing().expect("routing").shard_id;
    let index = MockPropertyIndex::default();
    index.set_equal_hits_for(
        uid_pid,
        alice_key,
        vec![
            PostingHit {
                shard_id: local_shard,
                vertex_id: u32::from(vid1),
            },
            PostingHit {
                shard_id: local_shard,
                vertex_id: u32::from(vid2),
            },
        ],
    );
    index.set_equal_hits_for(
        email_pid,
        email_key,
        vec![
            PostingHit {
                shard_id: local_shard,
                vertex_id: u32::from(vid2),
            },
            PostingHit {
                shard_id: local_shard,
                vertex_id: u32::from(vid3),
            },
        ],
    );
    let plan = plan(vec![PlanOp::IndexIntersection {
        variable: "n".into(),
        scans: vec![
            IndexScanSpec {
                property: "uid".into(),
                value: ScanValue::Literal(Value::Text("alice".into())),
                cmp: CmpOp::Eq,
            },
            IndexScanSpec {
                property: "email".into(),
                value: ScanValue::Literal(Value::Text("alice@example.com".into())),
                cmp: CmpOp::Eq,
            },
        ],
        property_projection: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("uid"),
        crate::test_labels::property_id_for_name("email"),
    ]);
    let rows = pollster::block_on(execute_plan_query_bindings(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("index intersection");

    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == vid2
    ));
    assert_eq!(index.intersection_calls.borrow().len(), 1);
    assert!(index.equal_calls.borrow().is_empty());
}

#[test]
fn index_intersection_empty_when_disjoint() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let _ = store
        .insert_vertex_named(
            ["IxA"],
            [
                ("uid", Value::Text("alice".into())),
                ("email", Value::Text("bob@example.com".into())),
            ],
        )
        .expect("vertex");
    let uid_pid = crate::test_labels::property_id_for_name("uid").raw();
    let email_pid = crate::test_labels::property_id_for_name("email").raw();
    let alice_key = value_to_index_key_bytes(&Value::Text("alice".into()))
        .expect("encode uid")
        .expect("sortable uid");
    let email_key = value_to_index_key_bytes(&Value::Text("bob@example.com".into()))
        .expect("encode email")
        .expect("sortable email");
    let local_shard = store.federation_routing().expect("routing").shard_id;
    let index = MockPropertyIndex::default();
    index.set_equal_hits_for(
        uid_pid,
        alice_key,
        vec![PostingHit {
            shard_id: local_shard,
            vertex_id: 1,
        }],
    );
    index.set_equal_hits_for(
        email_pid,
        email_key,
        vec![PostingHit {
            shard_id: local_shard,
            vertex_id: 2,
        }],
    );
    let plan = plan(vec![PlanOp::IndexIntersection {
        variable: "n".into(),
        scans: vec![
            IndexScanSpec {
                property: "uid".into(),
                value: ScanValue::Literal(Value::Text("alice".into())),
                cmp: CmpOp::Eq,
            },
            IndexScanSpec {
                property: "email".into(),
                value: ScanValue::Literal(Value::Text("bob@example.com".into())),
                cmp: CmpOp::Eq,
            },
        ],
        property_projection: None,
    }]);

    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("uid"),
        crate::test_labels::property_id_for_name("email"),
    ]);
    let rows = pollster::block_on(execute_plan_query_bindings(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("empty intersection");

    assert!(rows.is_empty());
}

#[test]
fn index_intersection_requires_index_client() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let plan = plan(vec![PlanOp::IndexIntersection {
        variable: "n".into(),
        scans: vec![
            IndexScanSpec {
                property: "uid".into(),
                value: ScanValue::Literal(Value::Text("alice".into())),
                cmp: CmpOp::Eq,
            },
            IndexScanSpec {
                property: "email".into(),
                value: ScanValue::Literal(Value::Text("alice@example.com".into())),
                cmp: CmpOp::Eq,
            },
        ],
        property_projection: None,
    }]);

    let err = pollster::block_on(execute_plan_query_bindings(
        &store,
        &plan,
        &params(),
        None,
        GqlExecutionContext::default(),
    ))
    .expect_err("missing index client");

    assert!(matches!(
        err,
        PlanQueryError::UnsupportedOp("IndexIntersection(no index client)")
    ));
}

#[test]
fn seeded_skip_leading_index_intersection_does_not_call_index() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let vid = store
        .insert_vertex_named(
            ["IxSeed"],
            [
                ("uid", Value::Text("alice".into())),
                ("email", Value::Text("alice@example.com".into())),
            ],
        )
        .expect("vertex");
    let plan = plan(vec![
        PlanOp::IndexIntersection {
            variable: "n".into(),
            scans: vec![
                IndexScanSpec {
                    property: "uid".into(),
                    value: ScanValue::Literal(Value::Text("alice".into())),
                    cmp: CmpOp::Eq,
                },
                IndexScanSpec {
                    property: "email".into(),
                    value: ScanValue::Literal(Value::Text("alice@example.com".into())),
                    cmp: CmpOp::Eq,
                },
            ],
            property_projection: None,
        },
        PlanOp::Project {
            columns: vec![project(var("n"), "n")],
            distinct: false,
        },
    ]);
    let mut seed = PlanRow::new();
    seed.insert("n".to_owned(), PlanBinding::Vertex(vid));
    let index = MockPropertyIndex::default();

    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
        vec![seed],
        true,
    ))
    .expect("seeded intersection skip");

    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == vid
    ));
    assert!(index.intersection_calls.borrow().is_empty());
}

#[test]
fn seeded_skip_leading_labeled_node_scan_and_index_scan_use_seed_only() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let vid = store
        .insert_vertex_named(["Person"], [("region", Value::Text("US".into()))])
        .expect("vertex");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("Person".into()),
            property_projection: None,
        },
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "region".into(),
            value: ScanValue::Literal(Value::Text("US".into())),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::Project {
            columns: vec![project(var("n"), "n")],
            distinct: false,
        },
    ]);
    let mut seed = PlanRow::new();
    seed.insert("n".to_owned(), PlanBinding::Vertex(vid));
    let index = MockPropertyIndex::default();

    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
        vec![seed],
        true,
    ))
    .expect("seeded compound skip");

    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == vid
    ));
    assert!(index.equal_calls.borrow().is_empty());
}

#[test]
fn seeded_skip_leading_labeled_node_scan_uses_seed_only() {
    let store = GraphStore::new();
    let vid1 = store
        .insert_vertex_named(["Person"], Vec::<(&str, Value)>::new())
        .expect("vertex 1");
    let _vid2 = store
        .insert_vertex_named(["Person"], Vec::<(&str, Value)>::new())
        .expect("vertex 2");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("Person".into()),
            property_projection: None,
        },
        PlanOp::Project {
            columns: vec![project(var("n"), "n")],
            distinct: false,
        },
    ]);
    let mut seed = PlanRow::new();
    seed.insert("n".to_owned(), PlanBinding::Vertex(vid1));
    let index = MockPropertyIndex::default();

    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
        vec![seed],
        true,
    ))
    .expect("seeded label skip");

    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == vid1
    ));
}

#[test]
fn seeded_skip_leading_index_scan_uses_seed_only() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let vid = store
        .insert_vertex_named(["IxSeedEq"], [("age", Value::Uint8(5))])
        .expect("vertex");
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "age".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        },
        PlanOp::Project {
            columns: vec![project(var("n"), "n")],
            distinct: false,
        },
    ]);
    let mut seed = PlanRow::new();
    seed.insert("n".to_owned(), PlanBinding::Vertex(vid));
    let index = MockPropertyIndex::default();

    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
        vec![seed],
        true,
    ))
    .expect("seeded equality skip");

    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == vid
    ));
    assert!(index.equal_calls.borrow().is_empty());
}

#[test]
fn seeded_skip_leading_node_scan_and_property_filter_uses_seed_only() {
    let store = GraphStore::new();
    let vid = store
        .insert_vertex_named(["Person"], [("region", Value::Text("US".into()))])
        .expect("vertex");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("Person".into()),
            property_projection: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "region")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Text("US".into())))),
            })],
            stage: 0,
        },
        PlanOp::Project {
            columns: vec![project(var("n"), "n")],
            distinct: false,
        },
    ]);
    let mut seed = PlanRow::new();
    seed.insert("n".to_owned(), PlanBinding::Vertex(vid));
    let index = MockPropertyIndex::default();

    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
        vec![seed],
        true,
    ))
    .expect("seeded node scan + property filter skip");

    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == vid
    ));
    assert!(index.equal_calls.borrow().is_empty());
    assert!(index.intersection_calls.borrow().is_empty());
}

#[test]
fn seeded_node_scan_reapplies_residual_property_filter_to_seed_rows() {
    // ADR 0029 regression: router seeds resolve only the label/index *anchor*; a residual
    // `PropertyFilter` on a non-indexed property is NOT applied when building seeds, so the
    // shard must re-apply it. Seeding both a matching and a non-matching vertex must yield
    // only the matching one (previously the filter was skipped and both were returned).
    let store = GraphStore::new();
    let matching = store
        .insert_vertex_named(["Person"], [("region", Value::Text("US".into()))])
        .expect("matching vertex");
    let other = store
        .insert_vertex_named(["Person"], [("region", Value::Text("EU".into()))])
        .expect("non-matching vertex");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("Person".into()),
            property_projection: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "region")),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Text("US".into())))),
            })],
            stage: 0,
        },
        PlanOp::Project {
            columns: vec![project(var("n"), "n")],
            distinct: false,
        },
    ]);
    let mut matching_seed = PlanRow::new();
    matching_seed.insert("n".to_owned(), PlanBinding::Vertex(matching));
    let mut other_seed = PlanRow::new();
    other_seed.insert("n".to_owned(), PlanBinding::Vertex(other));
    let index = MockPropertyIndex::default();

    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
        vec![matching_seed, other_seed],
        true,
    ))
    .expect("seeded node scan re-applies residual filter");

    assert_eq!(rows.len(), 1, "non-matching seed must be filtered out");
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == matching
    ));
    assert!(index.equal_calls.borrow().is_empty());
}

#[test]
fn seeded_skip_leading_label_intersection_plan_uses_seed_only() {
    let store = GraphStore::new();
    let vid = store
        .insert_vertex_named(["Person", "Employee"], Vec::<(&str, Value)>::new())
        .expect("vertex with both labels");
    let _person_only = store
        .insert_vertex_named(["Person"], Vec::<(&str, Value)>::new())
        .expect("person only");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "n".into(),
            label: Some("Person".into()),
            property_projection: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::IsLabeled {
                expr: Box::new(Expr::var("n")),
                label: LabelExpr::Name("Employee".into()),
                negated: false,
            })],
            stage: 0,
        },
        PlanOp::Project {
            columns: vec![project(var("n"), "n")],
            distinct: false,
        },
    ]);
    let mut seed = PlanRow::new();
    seed.insert("n".to_owned(), PlanBinding::Vertex(vid));
    let index = MockPropertyIndex::default();

    let rows = pollster::block_on(execute_plan_query_bindings_with_initial_rows(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
        vec![seed],
        true,
    ))
    .expect("seeded label intersection skip");

    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].get("n"),
        Some(PlanBinding::Vertex(id)) if *id == vid
    ));
}

/// Regression for the filter-pushdown stale-index defect found while landing edge
/// STARTS WITH: with a residual one-sided range predicate present, two moves in one
/// pass used to drag `IsLabeled(b)` ahead of the `EdgeBindEndpoints` that binds it,
/// failing execution with MissingBinding { variable: "b" }. The same query shape
/// anchors through EdgeIndexScan{Ge}, so it guards the shared leading-edge path.
#[test]
fn parsed_edge_range_path_level_where_binds_projected_rows() {
    let _weight_index = crate::test_labels::enter_indexed_edge_property_named("weight");
    let (store, _) = edge_inlist_store();
    let input = "MATCH (a:EdgeInA)-[e:EdgeInRel]->(b:EdgeInB) WHERE e.weight >= 7 RETURN b";
    let plan = plan_with_edge_inlist_stats(input);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("range path-level WHERE must execute after pushdown ordering fix");
    assert_eq!(result.rows.len(), 2, "weights 7 and 9 each bind one row");
}

// ════════════════════════════════════════════════════════════════════════════════
// ADR 0081 Slice A: index-ordered ORDER BY delivery
// ════════════════════════════════════════════════════════════════════════════════

use super::super::context::QueryExprEvaluator;

fn ordered_delivery_stats() -> gleaph_gql_planner::stats::TableStats {
    let mut stats = gleaph_gql_planner::stats::TableStats::default();
    stats.label_cardinality.insert("OrdDel".to_string(), 100);
    stats.range_indexed_vertex_properties.insert("score".into());
    stats
}

/// Seed vertices with `score` = 10..=50 and a mock index serving the `score >= min`
/// interval hits in ascending encoded-key order (the real posting-walk contract).
fn ordered_delivery_fixture(min_score: i64) -> (GraphStore, MockPropertyIndex) {
    let store = GraphStore::new();
    configure_test_index(&store);
    let mut hits: Vec<(Vec<u8>, PostingHit)> = Vec::new();
    for score in [10i64, 20, 30, 40, 50] {
        let vid = store
            .insert_vertex_named(["OrdDel"], [("score", Value::Int64(score))])
            .expect("insert vertex");
        if score >= min_score {
            let key = value_to_index_key_bytes(&Value::Int64(score))
                .unwrap()
                .expect("encodable score");
            hits.push((
                key,
                PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: u32::try_from(u64::from(vid)).expect("vertex id"),
                },
            ));
        }
    }
    hits.sort_by(|left, right| left.0.cmp(&right.0));
    let index = MockPropertyIndex::default();
    *index.range_hits.borrow_mut() = hits.into_iter().map(|(_, hit)| hit).collect();
    (store, index)
}

/// E2E: the task's canonical shape. The planner elides nothing here because LIMIT is
/// present, so TopK carries the ordering; execution must return globally ascending
/// scores truncated to the limit, deterministically across runs.
#[test]
fn ordered_delivery_range_limit_rows_ascending_and_deterministic() {
    let input =
        "MATCH (v:OrdDel) WHERE v.score >= 20 RETURN v.score AS score ORDER BY score LIMIT 2";
    let plan = plan_query_with_table_stats(input, &ordered_delivery_stats());
    assert!(
        matches!(
            plan.ops.first(),
            Some(PlanOp::IndexScan {
                ordered_by_sort: Some(_),
                ..
            })
        ),
        "eligible plan must carry intent: {:?}",
        plan.ops
    );
    assert!(
        plan.ops.iter().any(|op| matches!(op, PlanOp::TopK { .. })),
        "Sort+Limit must fuse into TopK: {:?}",
        plan.ops
    );

    let (_store, index) = ordered_delivery_fixture(20);
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("score"),
    ]);
    let first = pollster::block_on(execute_plan_query(
        &_store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("ordered delivery execution");
    let second = pollster::block_on(execute_plan_query(
        &_store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("ordered delivery rerun");

    let expected: Vec<Value> = [Value::Int64(20), Value::Int64(30)].to_vec();
    assert_eq!(
        first
            .rows
            .iter()
            .map(|row| row.get("score").cloned().expect("score"))
            .collect::<Vec<_>>(),
        expected,
        "rows must be globally ascending and truncated to the limit"
    );
    assert_eq!(
        first.rows, second.rows,
        "tie-free output must be deterministic"
    );
}

/// Without a limit the eligible Sort is elided entirely; rows stream out in scan order.
#[test]
fn ordered_delivery_without_limit_elides_sort_and_streams_scan_order() {
    let input = "MATCH (v:OrdDel) WHERE v.score >= 30 RETURN v.score AS score ORDER BY score";
    let plan = plan_query_with_table_stats(input, &ordered_delivery_stats());
    assert_eq!(
        scan_intent_of(&plan),
        Some("score".to_string()),
        "intent must be recorded"
    );
    assert!(
        !plan.ops.iter().any(|op| matches!(op, PlanOp::Sort { .. })),
        "Sort must be elided for the no-limit shape: {:?}",
        plan.ops
    );

    let (_store, index) = ordered_delivery_fixture(30);
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("score"),
    ]);
    let result = pollster::block_on(execute_plan_query(
        &_store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("ordered delivery execution");
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| row.get("score").cloned().expect("score"))
            .collect::<Vec<_>>(),
        vec![Value::Int64(30), Value::Int64(40), Value::Int64(50)]
    );
}

fn scan_intent_of(plan: &PhysicalPlan) -> Option<String> {
    match plan.ops.first() {
        Some(PlanOp::IndexScan {
            ordered_by_sort: Some(prop),
            ..
        }) => Some(prop.to_string()),
        _ => None,
    }
}

/// Red proof: disabling eligibility (planning without index statistics) yields the same
/// ROW SET through the residual NodeScan + full-sort path. Order equality is only
/// claimed for the tie-free deterministic runs above, not across access paths.
#[test]
fn ordered_delivery_row_set_matches_residual_path() {
    let input = "MATCH (v:OrdDel) WHERE v.score >= 20 RETURN v.score AS score ORDER BY score";
    let eligible = plan_query_with_table_stats(input, &ordered_delivery_stats());
    let residual =
        plan_query_with_table_stats(input, &gleaph_gql_planner::stats::TableStats::default());

    let (store, index) = ordered_delivery_fixture(20);
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("score"),
    ]);
    let run = |plan: &PhysicalPlan| {
        pollster::block_on(execute_plan_query(
            &store,
            plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execution")
    };
    let mut delivered = run(&eligible)
        .rows
        .iter()
        .map(|row| row.get("score").cloned().expect("score"))
        .collect::<Vec<_>>();
    let mut residual_rows = run(&residual)
        .rows
        .iter()
        .map(|row| row.get("score").cloned().expect("score"))
        .collect::<Vec<_>>();
    delivered.sort_by_key(|value| match value {
        Value::Int64(v) => *v,
        other => panic!("expected int score, got {other:?}"),
    });
    residual_rows.sort_by_key(|value| match value {
        Value::Int64(v) => *v,
        other => panic!("expected int score, got {other:?}"),
    });
    assert_eq!(delivered, residual_rows, "row sets must be identical");
}

/// Gate routing: with the intent present execution takes the ordered path; stripping
/// the intent from the same plan falls back to the full sort and returns identical rows.
#[test]
fn topk_gate_routes_between_ordered_and_fallback_paths() {
    use gleaph_gql_planner::ordered_delivery::mark_leading_index_scan_ordered;

    let input =
        "MATCH (v:OrdDel) WHERE v.score >= 10 RETURN v.score AS score ORDER BY score LIMIT 3";
    let mut plan = plan_query_with_table_stats(input, &ordered_delivery_stats());
    // Rebuild the leading op without intent, execute both variants.
    let mut stripped = plan.clone();
    if let Some(PlanOp::IndexScan {
        ordered_by_sort, ..
    }) = stripped.ops.first_mut()
    {
        *ordered_by_sort = None;
    }
    mark_leading_index_scan_ordered(&mut plan.ops, "score".into());

    let (_store, index) = ordered_delivery_fixture(10);
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("score"),
    ]);
    for plan_variant in [&plan, &stripped] {
        let result = pollster::block_on(execute_plan_query(
            &_store,
            plan_variant,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("topk execution");
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row.get("score").cloned().expect("score"))
                .collect::<Vec<_>>(),
            vec![Value::Int64(10), Value::Int64(20), Value::Int64(30)]
        );
    }
}

// ─── Tie-group boundary rule over verified-ascending input ─────────────────────

fn keyed_row(key: i64) -> PlanRow {
    let mut row = PlanRow::new();
    row.insert("k".to_string(), PlanBinding::Value(Value::Int64(key)));
    row
}

fn key_of(row: &PlanRow) -> i64 {
    match row.get("k") {
        Some(PlanBinding::Value(Value::Int64(v))) => *v,
        other => panic!("expected int key binding, got {other:?}"),
    }
}

static TOPK_EVAL_STORE: std::sync::OnceLock<GraphStore> = std::sync::OnceLock::new();

fn ordered_topk_evaluator(parameters: &BTreeMap<String, Value>) -> QueryExprEvaluator<'_> {
    QueryExprEvaluator {
        store: TOPK_EVAL_STORE.get_or_init(GraphStore::new),
        parameters,
        aggregate_specs: None,
        caller: None,
        resolved_labels: None,
        resolved_properties: None,
        element_id_key: gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture(),
    }
}

fn literal(value: i64) -> Expr {
    Expr::new(ExprKind::Literal(Value::Int64(value)))
}

#[test]
fn topk_ordered_stops_at_first_strictly_greater_after_survivors() {
    let parameters = params();
    let evaluator = ordered_topk_evaluator(&parameters);
    // Survivors [1,2,3]; the tail holds strictly greater keys only — consumption must
    // stop at 7 and never need to look at it or beyond.
    let rows: Vec<PlanRow> = [1, 2, 3, 7, 8, 9].iter().copied().map(keyed_row).collect();
    let ob = order_by(vec![sort_item(var("k"), None, None)]);
    let result =
        super::super::topk_ordered_input(&evaluator, rows, &ob, 3, 0).expect("topk ordered");
    assert_eq!(result.iter().map(key_of).collect::<Vec<_>>(), vec![1, 2, 3]);
}

#[test]
fn topk_ordered_keeps_consuming_through_closing_tie_group() {
    let parameters = params();
    let evaluator = ordered_topk_evaluator(&parameters);
    // Boundary survivor key is 1 with k=2; equal keys keep flowing until the strict
    // boundary (the 2), so the group is closed before consumption stops.
    let rows: Vec<PlanRow> = [1, 1, 1, 1, 2, 9].iter().copied().map(keyed_row).collect();
    let ob = order_by(vec![sort_item(var("k"), None, None)]);
    let result =
        super::super::topk_ordered_input(&evaluator, rows, &ob, 2, 0).expect("topk ordered");
    assert_eq!(result.iter().map(key_of).collect::<Vec<_>>(), vec![1, 1]);
}

#[test]
fn topk_ordered_offset_skips_within_merged_stream() {
    let parameters = params();
    let evaluator = ordered_topk_evaluator(&parameters);
    let rows: Vec<PlanRow> = [1, 2, 3, 4, 5].iter().copied().map(keyed_row).collect();
    let ob = order_by(vec![sort_item(var("k"), None, None)]);
    let result =
        super::super::topk_ordered_input(&evaluator, rows, &ob, 1, 2).expect("topk ordered");
    assert_eq!(result.iter().map(key_of).collect::<Vec<_>>(), vec![3]);
}

#[test]
fn topk_ordered_limit_above_row_count_returns_all_and_empty_input_yields_empty() {
    let parameters = params();
    let evaluator = ordered_topk_evaluator(&parameters);
    let ob = order_by(vec![sort_item(var("k"), None, None)]);
    let rows: Vec<PlanRow> = [1, 2].iter().copied().map(keyed_row).collect();
    let result =
        super::super::topk_ordered_input(&evaluator, rows, &ob, 5, 0).expect("topk ordered");
    assert_eq!(result.len(), 2);

    let empty = super::super::topk_ordered_input(&evaluator, Vec::new(), &ob, 5, 0)
        .expect("topk ordered empty");
    assert!(empty.is_empty(), "empty index must bind no rows");
}

#[test]
fn topk_ordered_falls_back_to_full_sort_if_order_contract_violated() {
    let parameters = params();
    let evaluator = ordered_topk_evaluator(&parameters);
    // A contract violation (descending pair) must fail safe into correct output.
    let rows: Vec<PlanRow> = [3, 1, 2].iter().copied().map(keyed_row).collect();
    let ob = order_by(vec![sort_item(var("k"), None, None)]);
    let result =
        super::super::topk_ordered_input(&evaluator, rows, &ob, 2, 0).expect("fallback sort");
    assert_eq!(result.iter().map(key_of).collect::<Vec<_>>(), vec![1, 2]);
}

/// The strict-boundary stop is observable: rows past the first strictly-greater key
/// are never evaluated, so a row whose key cannot participate in the ordering must
/// not fail execution once the boundary has closed.
#[test]
fn topk_ordered_never_consumes_rows_past_the_strict_boundary() {
    let parameters = params();
    let evaluator = ordered_topk_evaluator(&parameters);
    let ob = order_by(vec![sort_item(var("k"), None, None)]);
    let mut rows: Vec<PlanRow> = [1, 2].iter().copied().map(keyed_row).collect();
    // Strictly greater survivor (closes the stream), then an incomparable poison key.
    rows.push(keyed_row(5));
    let mut poison = PlanRow::new();
    poison.insert(
        "k".to_string(),
        PlanBinding::Value(Value::Text("poison".into())),
    );
    rows.push(poison);

    let result =
        super::super::topk_ordered_input(&evaluator, rows, &ob, 2, 0).expect("stop at boundary");
    assert_eq!(result.iter().map(key_of).collect::<Vec<_>>(), vec![1, 2]);
}

/// Equal keys must NOT trigger the stop: consumption continues through the closing
/// tie group, which is observable because the poison row right after the group is
/// reached and fails the ordering comparison.
#[test]
fn topk_ordered_does_not_stop_inside_a_tie_group() {
    let parameters = params();
    let evaluator = ordered_topk_evaluator(&parameters);
    let ob = order_by(vec![sort_item(var("k"), None, None)]);
    let mut rows: Vec<PlanRow> = vec![keyed_row(1)];
    let mut poison = PlanRow::new();
    poison.insert(
        "k".to_string(),
        PlanBinding::Value(Value::Text("poison".into())),
    );
    rows.push(poison);

    let err = super::super::topk_ordered_input(&evaluator, rows, &ob, 1, 0)
        .expect_err("tie group must keep consuming into the incomparable row");
    assert!(
        matches!(err, PlanQueryError::IncomparableSortValues { .. }),
        "expected incomparable tie-group continuation, got {err:?}"
    );
}

/// Residual filter + TopK early exit must deliver exactly the first `k` rows of the
/// fully sorted survivor stream — including duplicate keys and a tie group that
/// straddles the truncation boundary.
#[test]
fn ordered_delivery_topk_residual_filter_equals_full_sort_prefix() {
    let input_limited = "MATCH (v:OrdDel) WHERE v.score >= 0 AND v.flag = 0 RETURN v.score AS score \
         ORDER BY score LIMIT 3";
    let input_unlimited = "MATCH (v:OrdDel) WHERE v.score >= 0 AND v.flag = 0 RETURN v.score AS score \
         ORDER BY score";
    let plan = plan_query_with_table_stats(input_limited, &ordered_delivery_stats());
    assert_eq!(
        scan_intent_of(&plan),
        Some("score".to_string()),
        "residual PropertyFilter must not block eligibility: {plan:?}"
    );
    let unlimited = plan_query_with_table_stats(input_unlimited, &ordered_delivery_stats());
    assert_eq!(scan_intent_of(&unlimited), Some("score".to_string()));

    // Survivors of `flag = 0` carry duplicated keys, and LIMIT 3 cuts inside the
    // `20` tie group (survivor stream: 10, 20, 20, 30, 30, 50, 50).
    let store = GraphStore::new();
    configure_test_index(&store);
    let rows: &[(i64, i64)] = &[
        (10, 0),
        (10, 1),
        (20, 0),
        (20, 0),
        (20, 1),
        (30, 0),
        (30, 0),
        (40, 1),
        (50, 0),
        (50, 0),
    ];
    let mut hits: Vec<(Vec<u8>, PostingHit)> = Vec::new();
    for (score, flag) in rows.iter().copied() {
        let vid = store
            .insert_vertex_named(
                ["OrdDel"],
                [("score", Value::Int64(score)), ("flag", Value::Int64(flag))],
            )
            .expect("insert vertex");
        let key = value_to_index_key_bytes(&Value::Int64(score))
            .unwrap()
            .expect("encodable score");
        hits.push((
            key,
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: u32::try_from(u64::from(vid)).expect("vertex id"),
            },
        ));
    }
    hits.sort_by(|left, right| left.0.cmp(&right.0));
    let index = MockPropertyIndex::default();
    *index.range_hits.borrow_mut() = hits.into_iter().map(|(_, hit)| hit).collect();

    let run = |plan: &PhysicalPlan| {
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
            crate::test_labels::property_id_for_name("score"),
        ]);
        pollster::block_on(execute_plan_query(
            &store,
            plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execution")
    };
    let scores = |result: &PlanQueryResult| -> Vec<i64> {
        result
            .rows
            .iter()
            .map(|row| match row.get("score") {
                Some(Value::Int64(v)) => *v,
                other => panic!("expected int score binding, got {other:?}"),
            })
            .collect()
    };

    let delivered = scores(&run(&plan));
    assert_eq!(
        delivered,
        vec![10, 20, 20],
        "early exit must reproduce the sorted prefix through the tie group at the cut"
    );

    // Prefix property: the no-limit ordered delivery streams every survivor in scan
    // order; its first three rows must equal the limited run exactly.
    let full_stream = scores(&run(&unlimited));
    assert_eq!(
        full_stream,
        vec![10, 20, 20, 30, 30, 50, 50],
        "no-limit delivery must stream all survivors in encoded-key order"
    );
    assert_eq!(&full_stream[..3], &delivered[..],);

    // Same top-k multiset as the intent-stripped fallback (full sort inside TopK).
    let mut stripped = plan.clone();
    if let Some(PlanOp::IndexScan {
        ordered_by_sort, ..
    }) = stripped.ops.first_mut()
    {
        *ordered_by_sort = None;
    }
    let mut fallback = scores(&run(&stripped));
    let mut reference = delivered.clone();
    fallback.sort_unstable();
    reference.sort_unstable();
    assert_eq!(
        fallback, reference,
        "fallback full sort must select the same top-k multiset"
    );
}
