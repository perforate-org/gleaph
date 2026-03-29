//! §20.21–§20.25 — String functions, numeric functions.
//!
//! GQL rules: concatenation, upperFunction, lowerFunction, trimFunction,
//! absFunction, modFunction, floorFunction, ceilingFunction.

use crate::section_tests::p;
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

// ── concatenation ───────────────────────────────────────────────────────
mod concatenation {
    use super::*;

    /// n.first || ' ' || n.last — string concatenation
    #[test]
    fn string_concat() {
        let prog = p("MATCH (n) RETURN n.first || ' ' || n.last");
        // Should be nested Concat: Concat(Concat(n.first, ' '), n.last)
        match &ret_expr(&prog).kind {
            ExprKind::Concat(_, _) => {
                // The outer is a Concat — that's the key structural check
            }
            other => panic!("expected Concat, got {other:?}"),
        }
    }
}

// ── string functions ────────────────────────────────────────────────────
mod string_functions {
    use super::*;

    /// UPPER(n.name)
    #[test]
    fn upper() {
        let prog = p("MATCH (n) RETURN UPPER(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::Upper(inner) => {
                assert!(
                    matches!(&inner.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "name")
                );
            }
            other => panic!("expected Upper, got {other:?}"),
        }
    }

    /// LOWER(n.name)
    #[test]
    fn lower() {
        let prog = p("MATCH (n) RETURN LOWER(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::Lower(inner) => {
                assert!(
                    matches!(&inner.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "name")
                );
            }
            other => panic!("expected Lower, got {other:?}"),
        }
    }

    /// TRIM(n.name) — basic trim without spec
    #[test]
    fn trim() {
        let prog = p("MATCH (n) RETURN TRIM(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::Trim {
                spec,
                trim_char,
                expr,
            } => {
                assert!(spec.is_none());
                assert!(trim_char.is_none());
                assert!(
                    matches!(&expr.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "name")
                );
            }
            other => panic!("expected Trim, got {other:?}"),
        }
    }

    /// CHAR_LENGTH(n.name)
    #[test]
    fn char_length() {
        let prog = p("MATCH (n) RETURN CHAR_LENGTH(n.name)");
        match &ret_expr(&prog).kind {
            ExprKind::CharLength { .. } => {}
            other => panic!("expected CharLength, got {other:?}"),
        }
    }

    /// BYTE_LENGTH(n.data)
    #[test]
    fn byte_length() {
        let prog = p("MATCH (n) RETURN BYTE_LENGTH(n.data)");
        match &ret_expr(&prog).kind {
            ExprKind::ByteLength { .. } => {}
            other => panic!("expected ByteLength, got {other:?}"),
        }
    }
}

// ── numeric functions ───────────────────────────────────────────────────
mod numeric_functions {
    use super::*;

    /// ABS(n.x)
    #[test]
    fn abs() {
        let prog = p("MATCH (n) RETURN ABS(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Abs(inner) => {
                assert!(
                    matches!(&inner.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "x")
                );
            }
            other => panic!("expected Abs, got {other:?}"),
        }
    }

    /// MOD(n.x, 3)
    #[test]
    fn modulo() {
        let prog = p("MATCH (n) RETURN MOD(n.x, 3)");
        match &ret_expr(&prog).kind {
            ExprKind::Mod(left, right) => {
                assert!(
                    matches!(&left.as_ref().kind, ExprKind::PropertyAccess { property, .. } if property == "x")
                );
                assert_eq!(*right.as_ref(), Expr::int(3));
            }
            other => panic!("expected Mod, got {other:?}"),
        }
    }

    /// FLOOR(n.x)
    #[test]
    fn floor() {
        let prog = p("MATCH (n) RETURN FLOOR(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Floor(_) => {}
            other => panic!("expected Floor, got {other:?}"),
        }
    }

    /// CEIL(n.x)
    #[test]
    fn ceil() {
        let prog = p("MATCH (n) RETURN CEIL(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Ceil(_) => {}
            other => panic!("expected Ceil, got {other:?}"),
        }
    }

    /// SQRT(n.x)
    #[test]
    fn sqrt() {
        let prog = p("MATCH (n) RETURN SQRT(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Sqrt(_) => {}
            other => panic!("expected Sqrt, got {other:?}"),
        }
    }

    /// EXP(n.x)
    #[test]
    fn exp() {
        let prog = p("MATCH (n) RETURN EXP(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Exp(_) => {}
            other => panic!("expected Exp, got {other:?}"),
        }
    }

    /// LN(n.x)
    #[test]
    fn ln() {
        let prog = p("MATCH (n) RETURN LN(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Ln(_) => {}
            other => panic!("expected Ln, got {other:?}"),
        }
    }

    /// LOG(2, n.x)
    #[test]
    fn log() {
        let prog = p("MATCH (n) RETURN LOG(2, n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Log(base, _) => {
                assert_eq!(*base.as_ref(), Expr::int(2));
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    /// LOG10(n.x)
    #[test]
    fn log10() {
        let prog = p("MATCH (n) RETURN LOG10(n.x)");
        match &ret_expr(&prog).kind {
            ExprKind::Log10(_) => {}
            other => panic!("expected Log10, got {other:?}"),
        }
    }

    /// POWER(n.x, 2)
    #[test]
    fn power() {
        let prog = p("MATCH (n) RETURN POWER(n.x, 2)");
        match &ret_expr(&prog).kind {
            ExprKind::Power(_, exp) => {
                assert_eq!(*exp.as_ref(), Expr::int(2));
            }
            other => panic!("expected Power, got {other:?}"),
        }
    }
}
