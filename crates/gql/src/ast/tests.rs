use crate::Value;
use crate::token::Span;
use crate::types::{EdgeDirection, LabelExpr};

use super::*;

#[test]
fn expr_convenience_constructors() {
    assert_eq!(
        Expr::int(42),
        Expr::new(ExprKind::Literal(Value::Int64(42)))
    );
    assert_eq!(
        Expr::string("hello"),
        Expr::new(ExprKind::Literal(Value::Text("hello".into())))
    );
    assert_eq!(
        Expr::bool(true),
        Expr::new(ExprKind::Literal(Value::Bool(true)))
    );
    assert_eq!(Expr::null(), Expr::new(ExprKind::Literal(Value::Null)));
    assert_eq!(Expr::var("x"), Expr::new(ExprKind::Variable("x".into())));
}

#[test]
fn object_name_simple() {
    let name = ObjectName::simple("test");
    assert_eq!(name.parts, vec!["test".to_string()]);
}

#[test]
fn object_name_qualified() {
    let name = ObjectName::qualified(vec!["catalog".into(), "schema".into(), "graph".into()]);
    assert_eq!(name.parts.len(), 3);
}

#[test]
fn node_pattern_bare() {
    let n = NodePattern::bare();
    assert_eq!(n.variable, None);
    assert_eq!(n.label, None);
    assert!(n.properties.is_empty());
    assert!(n.where_clause.is_none());
}

#[test]
fn composite_query_single() {
    let q = CompositeQueryExpr::single(LinearQueryStatement {
        span: Span::DUMMY,
        at_schema: None,
        prefix_bindings: vec![],
        parts: vec![],
        result: Some(ResultStatement::Return(Box::new(ReturnStatement {
            span: Span::DUMMY,
            set_quantifier: SetQuantifier::None,
            body: ReturnBody::Star,
        }))),
    });
    assert!(q.rest.is_empty());
}

#[test]
fn value_type_not_null_wrapping() {
    let ty = ValueType::NotNull(Box::new(ValueType::Int64 {
        keyword: Keyword::new("INT64"),
    }));
    match &ty {
        ValueType::NotNull(inner) => assert_eq!(
            **inner,
            ValueType::Int64 {
                keyword: Keyword::new("INT64")
            }
        ),
        _ => panic!("Expected NotNull"),
}
}

#[test]
fn set_op_display() {
    assert_eq!(format!("{}", SetOp::Union), "UNION");
    assert_eq!(format!("{}", SetOp::UnionAll), "UNION ALL");
    assert_eq!(format!("{}", SetOp::Otherwise), "OTHERWISE");
}

#[test]
fn binary_op_display() {
    assert_eq!(format!("{}", BinaryOp::Add), "+");
}

#[test]
fn cmp_op_display() {
    assert_eq!(format!("{}", CmpOp::Ne), "<>");
    assert_eq!(format!("{}", CmpOp::Le), "<=");
}

#[test]
fn path_quantifier_clone() {
    let q = PathQuantifier::Range {
        lower: 1,
        upper: Some(5),
    };
    assert_eq!(q.clone(), q);
}

#[test]
fn edge_pattern_with_direction() {
    let e = EdgePattern {
        span: Span::DUMMY,
        direction: EdgeDirection::PointingRight,
        variable: Some("e".into()),
        is_or_colon: Some(IsOrColon::Colon),
        label: Some(LabelExpr::Name("KNOWS".into())),
        properties: vec![],
        where_clause: None,
    };
    assert_eq!(e.direction, EdgeDirection::PointingRight);
}

#[test]
fn value_type_list_with_element_type() {
    let ty = ValueType::List {
        keyword: Keyword::new("LIST"),
        element_type: Box::new(ValueType::Int32 {
            keyword: Keyword::new("INT32"),
        }),
        max_length: Some(100),
    };
    match &ty {
        ValueType::List {
            element_type,
            max_length,
            ..
        } => {
            assert_eq!(
                **element_type,
                ValueType::Int32 {
                    keyword: Keyword::new("INT32")
                }
            );
            assert_eq!(*max_length, Some(100));
        }
        _ => panic!("Expected List"),
}
}

#[test]
fn value_type_record() {
    let ty = ValueType::Record {
        record_keyword: true,
        fields: vec![
            RecordFieldType {
                span: Span::DUMMY,
                name: "name".into(),
                typed_prefix: TypedPrefix::None,
                value_type: ValueType::String {
                    min_length: None,
                    max_length: None,
                },
            },
            RecordFieldType {
                span: Span::DUMMY,
                name: "age".into(),
                typed_prefix: TypedPrefix::None,
                value_type: ValueType::Int32 {
                    keyword: Keyword::new("INT32"),
                },
            },
        ],
    };
    match &ty {
        ValueType::Record { fields, .. } => assert_eq!(fields.len(), 2),
        _ => panic!("Expected Record"),
}
}

#[test]
fn graph_pattern_simple() {
    let gp = GraphPattern::simple(vec![]);
    assert!(gp.match_mode.is_none());
    assert!(gp.paths.is_empty());
    assert!(gp.keep.is_none());
    assert!(gp.where_clause.is_none());
}

#[test]
fn match_statement_optional() {
    let m = MatchStatement {
        span: Span::DUMMY,
        optional: true,
        graph_name: None,
        pattern: GraphPattern::simple(vec![]),
        yield_items: None,
    };
    assert!(m.optional);
}

#[test]
fn delete_statement_detach() {
    let d = DeleteStatement {
        span: Span::DUMMY,
        detach: DeleteDetach::Detach,
        items: vec![Expr::new(ExprKind::Variable("n".into()))],
    };
    assert_eq!(d.detach, DeleteDetach::Detach);
    assert_eq!(
        d.items,
        vec![Expr::new(ExprKind::Variable("n".to_string()))]
    );
}

#[test]
fn aggregate_func_copy() {
    let f = AggregateFunc::PercentileCont;
    let f2 = f;
    assert_eq!(f, f2);
}

#[test]
fn normal_form_copy() {
    let nf = NormalForm::NFKD;
    let nf2 = nf;
    assert_eq!(nf, nf2);
}

#[test]
fn trim_spec_copy() {
    let ts = TrimSpec::Both;
    let ts2 = ts;
    assert_eq!(ts, ts2);
}

    // ── Display impl coverage ────────────────────────────────────────

#[test]
fn binary_op_display_all() {
    assert_eq!(format!("{}", BinaryOp::Add), "+");
    assert_eq!(format!("{}", BinaryOp::Sub), "-");
    assert_eq!(format!("{}", BinaryOp::Mul), "*");
    assert_eq!(format!("{}", BinaryOp::Div), "/");
}

#[test]
fn cmp_op_display_all() {
    assert_eq!(format!("{}", CmpOp::Eq), "=");
    assert_eq!(format!("{}", CmpOp::Ne), "<>");
    assert_eq!(format!("{}", CmpOp::Lt), "<");
    assert_eq!(format!("{}", CmpOp::Le), "<=");
    assert_eq!(format!("{}", CmpOp::Gt), ">");
    assert_eq!(format!("{}", CmpOp::Ge), ">=");
}

#[test]
fn unary_op_display_all() {
    assert_eq!(format!("{}", UnaryOp::Neg), "-");
    assert_eq!(format!("{}", UnaryOp::Pos), "+");
}

#[test]
fn set_op_display_all() {
    assert_eq!(format!("{}", SetOp::Union), "UNION");
    assert_eq!(format!("{}", SetOp::UnionAll), "UNION ALL");
    assert_eq!(format!("{}", SetOp::UnionDistinct), "UNION DISTINCT");
    assert_eq!(format!("{}", SetOp::Except), "EXCEPT");
    assert_eq!(format!("{}", SetOp::ExceptAll), "EXCEPT ALL");
    assert_eq!(format!("{}", SetOp::ExceptDistinct), "EXCEPT DISTINCT");
    assert_eq!(format!("{}", SetOp::Intersect), "INTERSECT");
    assert_eq!(format!("{}", SetOp::IntersectAll), "INTERSECT ALL");
    assert_eq!(
        format!("{}", SetOp::IntersectDistinct),
        "INTERSECT DISTINCT"
    );
    assert_eq!(format!("{}", SetOp::Otherwise), "OTHERWISE");
}

    // ── Keyword impls ────────────────────────────────────────────────

#[test]
fn keyword_equality_ignores_content() {
    let k1 = Keyword::new("INT");
    let k2 = Keyword::new("INTEGER");
    assert_eq!(k1, k2, "Keyword should always compare equal");
}

#[test]
fn keyword_debug() {
    let k = Keyword::new("BIGINT");
    let dbg = format!("{:?}", k);
    assert!(dbg.contains("BIGINT"));
}

#[test]
fn keyword_clone() {
    let k = Keyword::new("FLOAT");
    let k2 = k.clone();
    assert_eq!(k, k2);
    assert_eq!(k2.0, "FLOAT");
}

    // ── StatementBlock::iter_statements ──────────────────────────────

#[test]
fn statement_block_iter_statements() {
    let block = StatementBlock {
        span: Span::DUMMY,
        first: Statement::Query(Box::new(CompositeQueryExpr::single(
            LinearQueryStatement::parts_only(vec![]),
        ))),
        next: vec![NextStatement {
            span: Span::DUMMY,
            yield_items: None,
            statement: Statement::Query(Box::new(CompositeQueryExpr::single(
                LinearQueryStatement::parts_only(vec![]),
            ))),
        }],
    };
    let count = block.iter_statements().count();
    assert_eq!(count, 2, "expected 2 statements");
}

    // ── Copy/Clone/PartialEq for small enums ────────────────────────

#[test]
fn transaction_end_copy() {
    let te = TransactionEnd::Commit;
    let te2 = te;
    assert_eq!(te, te2);
    assert_eq!(TransactionEnd::Rollback, TransactionEnd::Rollback);
}

#[test]
fn transaction_access_mode_copy() {
    let m = TransactionAccessMode::ReadOnly;
    let m2 = m;
    assert_eq!(m, m2);
    assert_ne!(m, TransactionAccessMode::ReadWrite);
}

#[test]
fn delete_detach_variants() {
    assert_eq!(DeleteDetach::Detach, DeleteDetach::Detach);
    assert_eq!(DeleteDetach::NoDetach, DeleteDetach::NoDetach);
    assert_eq!(DeleteDetach::Unspecified, DeleteDetach::Unspecified);
    assert_ne!(DeleteDetach::Detach, DeleteDetach::NoDetach);
}

#[test]
fn set_quantifier_copy() {
    let q = SetQuantifier::Distinct;
    let q2 = q;
    assert_eq!(q, q2);
    assert_ne!(q, SetQuantifier::All);
    assert_ne!(q, SetQuantifier::None);
}

#[test]
fn sort_direction_variants() {
    assert_eq!(SortDirection::Asc, SortDirection::Asc);
    assert_eq!(SortDirection::Desc, SortDirection::Desc);
    assert_eq!(SortDirection::Ascending, SortDirection::Ascending);
    assert_eq!(SortDirection::Descending, SortDirection::Descending);
    assert_ne!(SortDirection::Asc, SortDirection::Desc);
}

#[test]
fn null_order_variants() {
    assert_eq!(NullOrder::First, NullOrder::First);
    assert_eq!(NullOrder::Last, NullOrder::Last);
    assert_ne!(NullOrder::First, NullOrder::Last);
}

#[test]
fn truth_value_variants() {
    assert_eq!(TruthValue::True, TruthValue::True);
    assert_eq!(TruthValue::False, TruthValue::False);
    assert_eq!(TruthValue::Unknown, TruthValue::Unknown);
    assert_ne!(TruthValue::True, TruthValue::False);
}

#[test]
fn normal_form_variants() {
    assert_eq!(NormalForm::NFC, NormalForm::NFC);
    assert_eq!(NormalForm::NFD, NormalForm::NFD);
    assert_eq!(NormalForm::NFKC, NormalForm::NFKC);
    assert_ne!(NormalForm::NFC, NormalForm::NFKD);
}

#[test]
fn string_fold_kind_variants() {
    assert_eq!(StringFoldKind::BTrim, StringFoldKind::BTrim);
    assert_eq!(StringFoldKind::LTrim, StringFoldKind::LTrim);
    assert_eq!(StringFoldKind::RTrim, StringFoldKind::RTrim);
    assert_ne!(StringFoldKind::BTrim, StringFoldKind::LTrim);
}

#[test]
fn aggregate_func_variants() {
    assert_eq!(AggregateFunc::Count, AggregateFunc::Count);
    assert_eq!(AggregateFunc::CountStar, AggregateFunc::CountStar);
    assert_eq!(AggregateFunc::Sum, AggregateFunc::Sum);
    assert_eq!(AggregateFunc::Avg, AggregateFunc::Avg);
    assert_eq!(AggregateFunc::Min, AggregateFunc::Min);
    assert_eq!(AggregateFunc::Max, AggregateFunc::Max);
    assert_eq!(AggregateFunc::Collect, AggregateFunc::Collect);
    assert_eq!(AggregateFunc::StddevSamp, AggregateFunc::StddevSamp);
    assert_eq!(AggregateFunc::StddevPop, AggregateFunc::StddevPop);
    assert_eq!(AggregateFunc::PercentileDisc, AggregateFunc::PercentileDisc);
}

#[test]
fn trim_spec_variants() {
    assert_eq!(TrimSpec::Leading, TrimSpec::Leading);
    assert_eq!(TrimSpec::Trailing, TrimSpec::Trailing);
    assert_ne!(TrimSpec::Leading, TrimSpec::Trailing);
}

#[test]
fn duration_qualifier_variants() {
    assert_eq!(
        DurationQualifier::YearToMonth,
        DurationQualifier::YearToMonth
    );
    assert_eq!(
        DurationQualifier::DayToSecond,
        DurationQualifier::DayToSecond
    );
    assert_ne!(
        DurationQualifier::YearToMonth,
        DurationQualifier::DayToSecond
    );
}

    // ── IsOrColon / TypedPrefix ──────────────────────────────────────

#[test]
fn is_or_colon_variants() {
    assert_eq!(IsOrColon::Is, IsOrColon::Is);
    assert_eq!(IsOrColon::Colon, IsOrColon::Colon);
    assert_ne!(IsOrColon::Is, IsOrColon::Colon);
}

#[test]
fn typed_prefix_variants() {
    assert_eq!(TypedPrefix::DoubleColon, TypedPrefix::DoubleColon);
    assert_eq!(TypedPrefix::Typed, TypedPrefix::Typed);
    assert_eq!(TypedPrefix::None, TypedPrefix::None);
    assert_ne!(TypedPrefix::DoubleColon, TypedPrefix::Typed);
}

    // ── PathOrPaths / GroupOrGroups ──────────────────────────────────

#[test]
fn path_or_paths_variants() {
    assert_eq!(PathOrPaths::Path, PathOrPaths::Path);
    assert_eq!(PathOrPaths::Paths, PathOrPaths::Paths);
    assert_ne!(PathOrPaths::Path, PathOrPaths::Paths);
}

#[test]
fn group_or_groups_variants() {
    assert_eq!(GroupOrGroups::Group, GroupOrGroups::Group);
    assert_eq!(GroupOrGroups::Groups, GroupOrGroups::Groups);
    assert_ne!(GroupOrGroups::Group, GroupOrGroups::Groups);
}

    // ── MatchMode variants ──────────────────────────────────────────

#[test]
fn match_mode_element_keyword_variants() {
    assert_eq!(
        MatchModeElementKeyword::Element,
        MatchModeElementKeyword::Element
    );
    assert_eq!(
        MatchModeElementKeyword::ElementBindings,
        MatchModeElementKeyword::ElementBindings
    );
    assert_ne!(
        MatchModeElementKeyword::Element,
        MatchModeElementKeyword::Elements
    );
}

#[test]
fn match_mode_edge_keyword_variants() {
    assert_eq!(MatchModeEdgeKeyword::Edge, MatchModeEdgeKeyword::Edge);
    assert_eq!(
        MatchModeEdgeKeyword::EdgeBindings,
        MatchModeEdgeKeyword::EdgeBindings
    );
    assert_eq!(
        MatchModeEdgeKeyword::Relationship,
        MatchModeEdgeKeyword::Relationship
    );
    assert_eq!(
        MatchModeEdgeKeyword::RelationshipBindings,
        MatchModeEdgeKeyword::RelationshipBindings
    );
    assert_ne!(
        MatchModeEdgeKeyword::Edge,
        MatchModeEdgeKeyword::Relationships
    );
}

    // ── PathMode variants ───────────────────────────────────────────

#[test]
fn path_mode_variants() {
    assert_eq!(PathMode::Walk, PathMode::Walk);
    assert_eq!(PathMode::Trail, PathMode::Trail);
    assert_eq!(PathMode::Simple, PathMode::Simple);
    assert_eq!(PathMode::Acyclic, PathMode::Acyclic);
    assert_ne!(PathMode::Walk, PathMode::Trail);
}

    // ── ValueType variants ──────────────────────────────────────────

#[test]
fn value_type_simple_variants() {
    assert_eq!(ValueType::Date, ValueType::Date);
    assert_eq!(ValueType::Time, ValueType::Time);
    assert_eq!(ValueType::DateTime, ValueType::DateTime);
    assert_eq!(ValueType::Timestamp, ValueType::Timestamp);
    assert_eq!(ValueType::Duration, ValueType::Duration);
    assert_eq!(
        ValueType::DurationYearToMonth,
        ValueType::DurationYearToMonth
    );
    assert_eq!(
        ValueType::DurationDayToSecond,
        ValueType::DurationDayToSecond
    );
    assert_eq!(ValueType::Path, ValueType::Path);
    assert_eq!(ValueType::Any, ValueType::Any);
    assert_eq!(ValueType::AnyValue, ValueType::AnyValue);
    assert_eq!(ValueType::AnyPropertyValue, ValueType::AnyPropertyValue);
    assert_eq!(ValueType::Nothing, ValueType::Nothing);
    assert_eq!(ValueType::Null, ValueType::Null);
    assert_eq!(ValueType::Float128, ValueType::Float128);
    assert_eq!(ValueType::Float256, ValueType::Float256);
}

#[test]
fn value_type_string_with_lengths() {
    let ty = ValueType::String {
        min_length: Some(1),
        max_length: Some(255),
    };
    match &ty {
        ValueType::String {
            min_length,
            max_length,
        } => {
            assert_eq!(*min_length, Some(1));
            assert_eq!(*max_length, Some(255));
        }
        _ => panic!("Expected String"),
}
}

#[test]
fn value_type_closed_dynamic_union() {
    let ty = ValueType::ClosedDynamicUnion(vec![
        ValueType::Int32 {
            keyword: Keyword::new("INT"),
        },
        ValueType::String {
            min_length: None,
            max_length: None,
        },
    ]);
    match &ty {
        ValueType::ClosedDynamicUnion(types) => assert_eq!(types.len(), 2),
        _ => panic!("Expected ClosedDynamicUnion"),
}
}

#[test]
fn value_type_decimal() {
    let ty = ValueType::Decimal {
        keyword: Keyword::new("DECIMAL"),
        precision: Some(10),
        scale: Some(2),
    };
    match &ty {
        ValueType::Decimal {
            precision, scale, ..
        } => {
            assert_eq!(*precision, Some(10));
            assert_eq!(*scale, Some(2));
        }
        _ => panic!("Expected Decimal"),
}
}

#[test]
fn value_type_float_precision() {
    let ty = ValueType::FloatPrecision {
        precision: 32,
        scale: Some(8),
    };
    match &ty {
        ValueType::FloatPrecision { precision, scale } => {
            assert_eq!(*precision, 32);
            assert_eq!(*scale, Some(8));
        }
        _ => panic!("Expected FloatPrecision"),
}
}

    // ── SchemaReference ─────────────────────────────────────────────

#[test]
fn schema_reference_variants() {
    let sr = SchemaReference::Current("HOME_SCHEMA".into());
    assert_eq!(sr, SchemaReference::Current("HOME_SCHEMA".into()));

    let sr2 = SchemaReference::Absolute(vec!["catalog".into(), "schema".into()]);
    match &sr2 {
        SchemaReference::Absolute(parts) => assert_eq!(parts.len(), 2),
        _ => panic!("Expected Absolute"),
}

    let sr3 = SchemaReference::Relative(vec!["..".into(), "other".into()]);
    match &sr3 {
        SchemaReference::Relative(parts) => assert_eq!(parts.len(), 2),
        _ => panic!("Expected Relative"),
}

    let sr4 = SchemaReference::Parameter("myParam".into());
    match &sr4 {
        SchemaReference::Parameter(name) => assert_eq!(name, "myParam"),
        _ => panic!("Expected Parameter"),
}
}

    // ── ProcedureBindingKind ────────────────────────────────────────

#[test]
fn procedure_binding_kind_variants() {
    assert_eq!(ProcedureBindingKind::Graph, ProcedureBindingKind::Graph);
    assert_eq!(ProcedureBindingKind::Table, ProcedureBindingKind::Table);
    assert_eq!(ProcedureBindingKind::Value, ProcedureBindingKind::Value);
}

    // ── StringPredicateKind ─────────────────────────────────────────

    #[cfg(feature = "cypher")]
#[test]
fn string_predicate_kind_variants() {
    assert_eq!(
        StringPredicateKind::StartsWith,
        StringPredicateKind::StartsWith
    );
    assert_eq!(StringPredicateKind::EndsWith, StringPredicateKind::EndsWith);
    assert_eq!(StringPredicateKind::Contains, StringPredicateKind::Contains);
    assert_eq!(StringPredicateKind::ILike, StringPredicateKind::ILike);
}

    // ── LinearQueryStatement parts_only ─────────────────────────────

#[test]
fn linear_query_parts_only() {
    let lq = LinearQueryStatement::parts_only(vec![]);
    assert!(lq.at_schema.is_none());
    assert!(lq.prefix_bindings.is_empty());
    assert!(lq.parts.is_empty());
    assert!(lq.result.is_none());
}

#[cfg(all(test, feature = "ast-rkyv-no-span"))]
mod rkyv_roundtrip_tests {
    use crate::token::Span;

    use super::*;
    use rkyv::{from_bytes_unchecked, rancor::Error, to_bytes};

#[test]
fn truth_value_roundtrip() {
    let v = TruthValue::Unknown;
    let bytes = to_bytes::<Error>(&v).unwrap();
    let back = unsafe { from_bytes_unchecked::<TruthValue, Error>(&bytes).unwrap() };
    assert_eq!(back, v);
}

#[test]
fn path_pattern_extension_roundtrip() {
    let pattern = PathPattern {
        span: Span::DUMMY,
        variable: None,
        prefix: None,
        expr: PathPatternExpr::Term(PathTerm {
            span: Span::DUMMY,
            factors: vec![],
        }),
        extensions: vec![PathPatternExtension {
            span: Span::DUMMY,
            name: ObjectName::simple("VENDOR_COST"),
            expr: Expr::var("e"),
        }],
    };
    let bytes = to_bytes::<Error>(&pattern).unwrap();
    let back = unsafe { from_bytes_unchecked::<PathPattern, Error>(&bytes).unwrap() };
    assert_eq!(back, pattern);
}
}
