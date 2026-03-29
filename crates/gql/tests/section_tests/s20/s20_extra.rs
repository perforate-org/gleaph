//! Additional expression coverage tests — targeting uncovered lines in parser/expr.rs.

use crate::section_tests::p;
use gleaph_gql::Value;
use gleaph_gql::ast::*;

/// Extract the first return item expression.
fn ret_expr(prog: &GqlProgram) -> &Expr {
    let b = crate::section_tests::body(prog);
    match &b.first {
        Statement::Query(cq) => match cq.left.result.as_ref().unwrap() {
            ResultStatement::Return(ret) => match &ret.body {
                ReturnBody::Items { items, .. } => &items[0].expr,
                other => panic!("expected Items, got {other:?}"),
            },
            other => panic!("expected Return, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── List slicing ────────────────────────────────────────────────────────

#[cfg(feature = "cypher")]
mod list_slice {
    use super::*;

    #[test]
    fn slice_to_only() {
        // expr[..to] — lines 91-98
        let prog = p("MATCH (n) RETURN n.items[..3]");
        match &ret_expr(&prog).kind {
            ExprKind::ListSlice { from, to, .. } => {
                assert!(from.is_none());
                assert!(to.is_some());
            }
            other => panic!("expected ListSlice, got {other:?}"),
        }
    }

    #[test]
    fn slice_from_to() {
        // expr[from..to] — line 105-106
        let prog = p("MATCH (n) RETURN n.items[1..3]");
        match &ret_expr(&prog).kind {
            ExprKind::ListSlice { from, to, .. } => {
                assert!(from.is_some());
                assert!(to.is_some());
            }
            other => panic!("expected ListSlice, got {other:?}"),
        }
    }
}

// ── Colon label predicate ───────────────────────────────────────────────

mod colon_label {
    use super::*;

    #[test]
    fn colon_label_expr() {
        // expr :Label — lines 231-238
        let prog = p("MATCH (n) RETURN n :Person");
        match &ret_expr(&prog).kind {
            ExprKind::IsLabeled { negated, .. } => {
                assert!(!negated);
            }
            other => panic!("expected IsLabeled, got {other:?}"),
        }
    }
}

// ── BigInt / ExactNumeric literals ──────────────────────────────────────

mod bigint_literals {
    use super::*;

    #[test]
    fn bigint_i128() {
        // Lines 382-385, 785
        let prog = p("MATCH (n) RETURN 99999999999999999999");
        match &ret_expr(&prog).kind {
            ExprKind::Literal(Value::Int128(_)) => {}
            other => panic!("expected Int128, got {other:?}"),
        }
    }

    #[test]
    fn bigint_u128() {
        // Line 789
        let prog = p("MATCH (n) RETURN 200000000000000000000000000000000000000");
        match &ret_expr(&prog).kind {
            ExprKind::Literal(Value::Uint128(_)) => {}
            other => panic!("expected Uint128, got {other:?}"),
        }
    }

    #[test]
    fn exact_numeric() {
        // Lines 396-401
        let prog = p("MATCH (n) RETURN 3.14M");
        match &ret_expr(&prog).kind {
            ExprKind::Literal(Value::Decimal(_)) => {}
            other => panic!("expected Decimal, got {other:?}"),
        }
    }
}

// ── UNKNOWN / NULL literals ─────────────────────────────────────────────

mod truth_values {
    use super::*;

    #[test]
    fn unknown_literal() {
        // Lines 479-480
        let prog = p("MATCH (n) RETURN UNKNOWN");
        match &ret_expr(&prog).kind {
            ExprKind::Literal(Value::Null) => {}
            other => panic!("expected Null, got {other:?}"),
        }
    }
}

// ── LIST/ARRAY constructors ─────────────────────────────────────────────

mod list_constructor {
    use super::*;

    #[test]
    fn empty_list_constructor() {
        // Line 494 (empty list)
        let prog = p("MATCH (n) RETURN LIST[]");
        match &ret_expr(&prog).kind {
            ExprKind::ListConstructor { items, .. } => {
                assert!(items.is_empty());
            }
            other => panic!("expected ListConstructor, got {other:?}"),
        }
    }
}

// ── PATH constructor ────────────────────────────────────────────────────

mod path_constructor {
    use super::*;

    #[test]
    fn path_constructor_three_elems() {
        // Lines 514, PATH[...] with odd elements
        let prog = p("MATCH (n) RETURN PATH[n, e, m]");
        match &ret_expr(&prog).kind {
            ExprKind::PathConstructor { elements } => {
                assert_eq!(elements.len(), 3);
            }
            other => panic!("expected PathConstructor, got {other:?}"),
        }
    }

    #[test]
    fn path_constructor_error_even() {
        // Lines 519-522 (error: even number of elements)
        let result = gleaph_gql::parser::parse("MATCH (n) RETURN PATH[n, e]");
        assert!(result.is_err());
    }
}

// ── PATH_LENGTH ─────────────────────────────────────────────────────────

mod path_length {
    use super::*;

    #[test]
    fn path_length_func() {
        // Lines 589-593
        let prog = p("MATCH (n) RETURN PATH_LENGTH(n.p)");
        match &ret_expr(&prog).kind {
            ExprKind::PathLength(_) => {}
            other => panic!("expected PathLength, got {other:?}"),
        }
    }
}

// ── LABEL (singular) ────────────────────────────────────────────────────

#[cfg(feature = "cypher")]
mod label_func {
    use super::*;

    #[test]
    fn label_singular() {
        // Lines 631-635
        let prog = p("MATCH (n) RETURN LABEL(n)");
        match &ret_expr(&prog).kind {
            ExprKind::Label(_) => {}
            other => panic!("expected Label, got {other:?}"),
        }
    }
}

// ── EXISTS with parenthesized match ─────────────────────────────────────

mod exists_expr {
    use super::*;

    #[test]
    fn exists_paren_match() {
        // Lines 919-938 — EXISTS(MATCH ...)
        let prog = p("MATCH (n) WHERE EXISTS(MATCH (m)-[:KNOWS]->(n)) RETURN n");
        // Just check it parses correctly
        let _b = crate::section_tests::body(&prog);
    }

    #[test]
    fn exists_brace_match() {
        // Lines 904-905 — EXISTS { MATCH ... RETURN ... }
        let prog = p("MATCH (n) WHERE EXISTS { MATCH (m) RETURN m } RETURN n");
        let _b = crate::section_tests::body(&prog);
    }
}

// ── Aggregate functions ─────────────────────────────────────────────────

mod aggregates {
    use super::*;

    #[test]
    fn stddev_samp() {
        // Line 1118
        let prog = p("MATCH (n) RETURN STDDEV_SAMP(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::StddevSamp),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn stddev_pop() {
        // Line 1120
        let prog = p("MATCH (n) RETURN STDDEV_POP(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::StddevPop),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn percentile_cont() {
        // Lines 1122, 1140-1141
        let prog = p("MATCH (n) RETURN PERCENTILE_CONT(n.x, 0.5)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, expr2, .. } => {
                assert_eq!(*func, AggregateFunc::PercentileCont);
                assert!(expr2.is_some());
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn percentile_disc() {
        // Line 1124
        let prog = p("MATCH (n) RETURN PERCENTILE_DISC(n.x, 0.9)");
        match &ret_expr(&prog).kind {
            ExprKind::Aggregate { func, .. } => assert_eq!(*func, AggregateFunc::PercentileDisc),
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn generic_function_no_args() {
        // Line 988 (empty args in generic function)
        let prog = p("MATCH (n) RETURN my_func()");
        match &ret_expr(&prog).kind {
            ExprKind::FunctionCall { args, .. } => assert!(args.is_empty()),
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }
}

// ── Numeric functions ───────────────────────────────────────────────────

mod numeric_functions {
    use super::*;

    #[test]
    fn ceiling_alias() {
        // Lines 1182-1186
        let prog = p("MATCH (n) RETURN CEILING(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Ceil(_) => {}
            other => panic!("expected Ceil, got {other:?}"),
        }
    }

    #[test]
    fn cot_function() {
        let prog = p("MATCH (n) RETURN COT(n.x)");
        assert!(matches!(&ret_expr(&prog).kind, ExprKind::Cot(_)));
    }

    #[test]
    fn sinh_function() {
        let prog = p("MATCH (n) RETURN SINH(n.x)");
        assert!(matches!(&ret_expr(&prog).kind, ExprKind::Sinh(_)));
    }

    #[test]
    fn cosh_function() {
        let prog = p("MATCH (n) RETURN COSH(n.x)");
        assert!(matches!(&ret_expr(&prog).kind, ExprKind::Cosh(_)));
    }

    #[test]
    fn tanh_function() {
        let prog = p("MATCH (n) RETURN TANH(n.x)");
        assert!(matches!(&ret_expr(&prog).kind, ExprKind::Tanh(_)));
    }

    #[cfg(feature = "sql-compat")]
    #[test]
    fn atan2_function() {
        // Lines 1253-1259
        let prog = p("MATCH (n) RETURN ATAN2(n.x, n.y)");
        match &ret_expr(&prog).kind {
            ExprKind::Atan2(_, _) => {}
            other => panic!("expected Atan2, got {other:?}"),
        }
    }

    #[cfg(feature = "sql-compat")]
    #[test]
    fn truncate_function() {
        // Lines 1265-1269
        let prog = p("MATCH (n) RETURN TRUNCATE(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Truncate { places, .. } => assert!(places.is_none()),
            other => panic!("expected Truncate, got {other:?}"),
        }
    }

    #[cfg(feature = "sql-compat")]
    #[test]
    fn truncate_with_places() {
        let prog = p("MATCH (n) RETURN TRUNCATE(n.x, 2)");
        match &ret_expr(&prog).kind {
            ExprKind::Truncate { places, .. } => assert!(places.is_some()),
            other => panic!("expected Truncate, got {other:?}"),
        }
    }

    #[cfg(feature = "sql-compat")]
    #[test]
    fn trunc_alias() {
        // Line 1263
        let prog = p("MATCH (n) RETURN TRUNC(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Truncate { .. } => {}
            other => panic!("expected Truncate, got {other:?}"),
        }
    }

    #[cfg(feature = "sql-compat")]
    #[test]
    fn round_function() {
        // Lines 1273-1277
        let prog = p("MATCH (n) RETURN ROUND(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Round { places, .. } => assert!(places.is_none()),
            other => panic!("expected Round, got {other:?}"),
        }
    }

    #[cfg(feature = "sql-compat")]
    #[test]
    fn round_with_places() {
        // Line 1287
        let prog = p("MATCH (n) RETURN ROUND(n.x, 2)");
        match &ret_expr(&prog).kind {
            ExprKind::Round { places, .. } => assert!(places.is_some()),
            other => panic!("expected Round, got {other:?}"),
        }
    }
}

// ── String functions ────────────────────────────────────────────────────

mod string_functions {
    use super::*;

    #[test]
    fn btrim() {
        // Lines 1358-1363
        let prog = p("MATCH (n) RETURN BTRIM(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::FoldString { kind, chars, .. } => {
                assert_eq!(*kind, StringFoldKind::BTrim);
                assert!(chars.is_none());
            }
            other => panic!("expected FoldString, got {other:?}"),
        }
    }

    #[test]
    fn btrim_with_chars() {
        let prog = p("MATCH (n) RETURN BTRIM(n.name, ' ')");
        match &ret_expr(&prog).kind {
            ExprKind::FoldString { kind, chars, .. } => {
                assert_eq!(*kind, StringFoldKind::BTrim);
                assert!(chars.is_some());
            }
            other => panic!("expected FoldString, got {other:?}"),
        }
    }

    #[test]
    fn ltrim() {
        // Line 1365
        let prog = p("MATCH (n) RETURN LTRIM(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::FoldString { kind, .. } => assert_eq!(*kind, StringFoldKind::LTrim),
            other => panic!("expected FoldString, got {other:?}"),
        }
    }

    #[test]
    fn rtrim() {
        // Lines 1367-1372
        let prog = p("MATCH (n) RETURN RTRIM(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::FoldString { kind, .. } => assert_eq!(*kind, StringFoldKind::RTrim),
            other => panic!("expected FoldString, got {other:?}"),
        }
    }

    #[test]
    fn normalize_nfkc() {
        // Line 1019-1021
        let prog = p("MATCH (n) RETURN NORMALIZE(n.name, NFKC)");
        match &ret_expr(&prog).kind {
            ExprKind::Normalize { form, .. } => assert_eq!(*form, NormalForm::NFKC),
            other => panic!("expected Normalize, got {other:?}"),
        }
    }

    #[test]
    fn normalize_nfkd() {
        let prog = p("MATCH (n) RETURN NORMALIZE(n.name, NFKD)");
        match &ret_expr(&prog).kind {
            ExprKind::Normalize { form, .. } => assert_eq!(*form, NormalForm::NFKD),
            other => panic!("expected Normalize, got {other:?}"),
        }
    }

    #[test]
    fn normalize_nfd() {
        // Line 1018
        let prog = p("MATCH (n) RETURN NORMALIZE(n.name, NFD)");
        match &ret_expr(&prog).kind {
            ExprKind::Normalize { form, .. } => assert_eq!(*form, NormalForm::NFD),
            other => panic!("expected Normalize, got {other:?}"),
        }
    }

    #[test]
    fn normalize_default() {
        // Line 1384 (no explicit form → NFC)
        let prog = p("MATCH (n) RETURN NORMALIZE(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::Normalize { form, .. } => assert_eq!(*form, NormalForm::NFC),
            other => panic!("expected Normalize, got {other:?}"),
        }
    }
}

// ── TRIM variants ───────────────────────────────────────────────────────

mod trim_variants {
    use super::*;

    #[test]
    fn trim_trailing() {
        // Line 1429
        let prog = p("MATCH (n) RETURN TRIM(TRAILING FROM n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::Trim {
                spec, trim_char, ..
            } => {
                assert_eq!(*spec, Some(TrimSpec::Trailing));
                assert!(trim_char.is_none());
            }
            other => panic!("expected Trim, got {other:?}"),
        }
    }

    #[test]
    fn trim_leading_from() {
        // Lines 1443-1449
        let prog = p("MATCH (n) RETURN TRIM(LEADING FROM n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::Trim {
                spec, trim_char, ..
            } => {
                assert_eq!(*spec, Some(TrimSpec::Leading));
                assert!(trim_char.is_none());
            }
            other => panic!("expected Trim, got {other:?}"),
        }
    }

    #[test]
    fn trim_list_function() {
        // Lines 1465-1469 (TRIM(list, count))
        let prog = p("MATCH (n) RETURN TRIM(n.items, 2)");
        match &ret_expr(&prog).kind {
            ExprKind::TrimList { .. } => {}
            other => panic!("expected TrimList, got {other:?}"),
        }
    }
}

// ── String predicates ───────────────────────────────────────────────────

#[cfg(feature = "cypher")]
mod string_predicates {
    use super::*;

    #[test]
    fn starts_with() {
        // Lines 1051-1053
        let prog = p("MATCH (n) WHERE n.name STARTS WITH 'A' RETURN n");
        let _b = crate::section_tests::body(&prog);
    }

    #[test]
    fn ends_with() {
        // Lines 1051-1053 alternate branch
        let prog = p("MATCH (n) WHERE n.name ENDS WITH 'z' RETURN n");
        let _b = crate::section_tests::body(&prog);
    }

    #[test]
    fn not_starts_with() {
        let prog = p("MATCH (n) WHERE n.name NOT STARTS WITH 'X' RETURN n");
        let _b = crate::section_tests::body(&prog);
    }
}

// ── IS predicate fallthrough ────────────────────────────────────────────

mod is_predicate {
    use super::*;

    #[test]
    fn is_not_null() {
        // Lines 223, 226 — IS NOT NULL (restore path)
        let prog = p("MATCH (n) WHERE n.x IS NOT NULL RETURN n");
        let _b = crate::section_tests::body(&prog);
    }
}
