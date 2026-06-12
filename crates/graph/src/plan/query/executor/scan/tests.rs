use super::super::test_support::*;
use crate::plan::query::executor::execute_plan_query_bindings_with_initial_rows;
use pollster;
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
    }]);

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
    }]);

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
    }]);

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
    assert!(matches!(
        &calls[0].1,
        PostingRangeRequest::Ge(bytes)
            if bytes == &value_to_index_key_bytes(&Value::Int64(5)).unwrap().unwrap()
    ));
}

#[test]
fn executes_list_range_index_scan_with_lookup_range() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let hit = store
        .insert_vertex_named(
            ["IndexScanListRange"],
            [("tags", Value::List(vec![Value::Int64(2)]))],
        )
        .expect("insert hit");
    let miss = store
        .insert_vertex_named(
            ["IndexScanListRange"],
            [("tags", Value::List(vec![Value::Int64(0)]))],
        )
        .expect("insert miss");
    let pid = crate::test_labels::property_id_for_name("tags").raw();
    let index = MockPropertyIndex::default();
    index.range_hits.borrow_mut().extend([
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: u32::try_from(u64::from(hit)).unwrap(),
        },
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: u32::try_from(u64::from(miss)).unwrap(),
        },
    ]);
    let bound = Value::List(vec![Value::Int64(1)]);
    let plan = plan(vec![
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Ge,
            property_projection: None,
        },
        PlanOp::PropertyFilter {
            predicates: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("n", "tags")),
                op: CmpOp::Ge,
                right: Box::new(Expr::new(ExprKind::Literal(bound.clone()))),
            })],
            stage: 0,
        },
    ]);

    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute list range index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert!(matches!(
        &calls[0].1,
        PostingRangeRequest::Ge(bytes)
            if bytes == &value_to_index_key_bytes(&bound).unwrap().unwrap()
    ));
    assert!(index.equal_calls.borrow().is_empty());
}

#[test]
fn executes_record_range_index_scan_with_lookup_range() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let hit = store
        .insert_vertex_named(
            ["IndexScanRecordRange"],
            [(
                "profile",
                Value::Record(vec![
                    ("a".into(), Value::Int64(1)),
                    ("b".into(), Value::Int64(1)),
                ]),
            )],
        )
        .expect("insert hit");
    let pid = crate::test_labels::property_id_for_name("profile").raw();
    let index = MockPropertyIndex::default();
    index.range_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(hit)).unwrap(),
    });
    let bound = Value::Record(vec![
        ("b".into(), Value::Int64(2)),
        ("a".into(), Value::Int64(1)),
    ]);
    let canonical_bound = Value::Record(vec![
        ("a".into(), Value::Int64(1)),
        ("b".into(), Value::Int64(2)),
    ]);
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "profile".into(),
        value: ScanValue::Literal(bound),
        cmp: CmpOp::Lt,
        property_projection: None,
    }]);

    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute record range index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert!(matches!(
        &calls[0].1,
        PostingRangeRequest::Lt(bytes)
            if bytes == &value_to_index_key_bytes(&canonical_bound).unwrap().unwrap()
    ));
    assert!(index.equal_calls.borrow().is_empty());
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
    }]);

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
fn executes_orderable_extension_range_index_scan() {
    let store = GraphStore::new();
    configure_test_index(&store);
    let bound = orderable_ext(7);
    store
        .insert_vertex_named(
            ["IndexScanExtensionRangeCatalog"],
            [("principal", Value::Text("catalog".into()))],
        )
        .expect("insert catalog vertex");
    let vid = store
        .insert_vertex_named(["IndexScanExtensionRange"], Vec::<(&str, Value)>::new())
        .expect("insert vertex");
    let pid = crate::test_labels::property_id_for_name("principal").raw();
    let index = MockPropertyIndex::default();
    index.range_hits.borrow_mut().push(PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: u32::try_from(u64::from(vid)).unwrap(),
    });
    let plan = plan(vec![PlanOp::IndexScan {
        variable: "n".into(),
        property: "principal".into(),
        value: ScanValue::Literal(bound.clone()),
        cmp: CmpOp::Ge,
        property_projection: None,
    }]);

    let result = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        Some(&index),
        GqlExecutionContext::default(),
    ))
    .expect("execute extension range index scan");

    assert_eq!(result.rows.len(), 1);
    let calls = index.range_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, pid);
    assert!(matches!(
        &calls[0].1,
        PostingRangeRequest::Ge(bytes)
            if bytes == &value_to_index_key_bytes(&bound).unwrap().unwrap()
    ));
    assert!(index.equal_calls.borrow().is_empty());
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
fn labeled_expand_limit_offset_pages_latest_edges() {
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
    assert_eq!(edge_stream_visits(), 2);

    reset_edge_stream_visits();
    let second = store
        .execute_plan_query(&second_page, &params(), GqlExecutionContext::default())
        .expect("execute second page");
    assert_eq!(edge_stream_visits(), 2);

    assert_eq!(text_column(&first, "b.name"), vec!["edge 4", "edge 3"]);
    assert_eq!(text_column(&second, "b.name"), vec!["edge 2", "edge 1"]);
}

#[test]
fn gleaph_sequence_asc_pages_labeled_edges_in_insertion_order() {
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
             ORDER BY GLEAPH.SEQUENCE(e) ASC LIMIT 2 OFFSET 1 RETURN b.name",
    );

    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute asc page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["seq edge 1", "seq edge 2"]
    );
}

#[test]
fn gleaph_sequence_desc_matches_default_labeled_edge_order() {
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
             ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 2 RETURN b.name",
    );

    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute desc page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["seq desc edge 3", "seq desc edge 2"]
    );
}

#[test]
fn gleaph_sequence_rejects_unlabeled_edge_pattern() {
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
             ORDER BY GLEAPH.SEQUENCE(e) ASC RETURN b",
    );

    let err = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect_err("unlabeled sequence order should fail");

    assert!(err.to_string().contains("single fixed edge label"), "{err}");
}

#[test]
fn unlabeled_directed_expand_limit_offset_uses_latest_edges() {
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

    reset_edge_stream_visits();
    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["unlabeled edge 2", "unlabeled edge 1"]
    );
    assert_eq!(edge_stream_visits(), 2);
}

#[test]
fn reverse_expand_limit_offset_uses_latest_in_edges() {
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

    reset_edge_stream_visits();
    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "a.name"),
        vec!["reverse edge 2", "reverse edge 1"]
    );
    assert_eq!(edge_stream_visits(), 2);
}

#[test]
fn undirected_expand_limit_offset_uses_latest_edges() {
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

    reset_edge_stream_visits();
    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["undirected edge 2", "undirected edge 1"]
    );
    assert_eq!(edge_stream_visits(), 2);
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

    reset_edge_stream_visits();
    let result = store
        .execute_plan_query(&page, &params(), GqlExecutionContext::default())
        .expect("execute page");

    assert_eq!(
        text_column(&result, "b.name"),
        vec!["filtered edge 4", "filtered edge 2"]
    );
    assert_eq!(edge_stream_visits(), 4);
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
            edge_payload_predicate: None,
            edge_vector_predicate: None,
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

    reset_edge_stream_visits();
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("indexed expand");

    assert_eq!(
        text_column(&result, "name"),
        vec!["indexed edge 4", "indexed edge 2"]
    );
    assert_eq!(edge_stream_visits(), 4);
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
    crate::test_labels::register_indexed_edge_property_named("weight");
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
fn leading_edge_bind_endpoints_hop_aux_returns_payload_bytes() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["LeadHopA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["LeadHopB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let label_id = crate::test_labels::edge_label_id_for_name("LeadHopRoad");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightRawU16,
        },
    );
    let weight_prop = crate::test_labels::property_id_for_name("weight");
    crate::test_labels::register_indexed_edge_property_named("weight");
    let payload = 7u16.to_le_bytes();
    let edge = store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &payload)
        .expect("edge");
    store
        .set_edge_property(edge, weight_prop, Value::Int64(7))
        .expect("edge property");

    let plan = plan(vec![
        PlanOp::EdgeIndexScan {
            variable: "e".into(),
            property: "weight".into(),
            value: ScanValue::Literal(Value::Int64(7)),
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
        Some(&Value::Bytes(payload.to_vec()))
    );
}

#[test]
fn leading_edge_bind_endpoints_honors_prebound_far_vertex() {
    let store = GraphStore::new();
    crate::test_labels::register_indexed_edge_property_named("weight");
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
