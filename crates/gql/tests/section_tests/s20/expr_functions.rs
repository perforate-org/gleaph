//! §20.21 — Numeric and string functions.

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

// ── Numeric functions ───────────────────────────────────────────────────

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

// ── String functions ────────────────────────────────────────────────────

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

// ── TRIM variants ───────────────────────────────────────────────────────

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

// ── String predicates (Cypher extensions) ───────────────────────────────

#[cfg(feature = "cypher")]
mod string_predicates {
    use super::*;

    #[test]
    fn starts_with() {
        let prog = p("MATCH (n) WHERE n.name STARTS WITH 'A' RETURN n");
        let _b = crate::section_tests::body(&prog);
    }

    #[test]
    fn ends_with() {
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

#[test]
fn is_not_null() {
    // Lines 223, 226 — IS NOT NULL (restore path)
    let prog = p("MATCH (n) WHERE n.x IS NOT NULL RETURN n");
    let _b = crate::section_tests::body(&prog);
}
