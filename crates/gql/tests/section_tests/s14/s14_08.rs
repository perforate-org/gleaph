//! §14.8 — For statement.
//!
//! GQL rules: forStatement, forItem, forItemAlias, forItemSource,
//! forOrdinalityOrOffset.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── forStatement ───────────────────────────────────────────────────────
//   : FOR forItem forOrdinalityOrOffset?
//   ;
mod for_statement {
    use super::*;

    /// FOR x IN [1, 2, 3] RETURN x — basic for
    #[test]
    fn basic_for() {
        let prog = p("FOR x IN [1, 2, 3] RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let for_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::For(_)));
            if let Some(SimpleQueryStatement::For(f)) = for_part {
                assert_eq!(f.variable, "x");
                assert!(f.ordinality.is_none());
            } else {
                panic!("expected SimpleQueryStatement::For in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// FOR x IN [1, 2, 3] WITH ORDINALITY i RETURN x, i — with ordinality
    #[test]
    fn with_ordinality() {
        let prog = p("FOR x IN [1, 2, 3] WITH ORDINALITY i RETURN x, i");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let for_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::For(_)));
            if let Some(SimpleQueryStatement::For(f)) = for_part {
                assert_eq!(f.variable, "x");
                let ord = f.ordinality.as_ref().expect("expected ordinality");
                assert!(!ord.offset_keyword);
                assert_eq!(ord.variable, "i");
            } else {
                panic!("expected SimpleQueryStatement::For in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// FOR x IN [1, 2, 3] WITH OFFSET o RETURN x, o — with offset
    #[test]
    fn with_offset() {
        let prog = p("FOR x IN [1, 2, 3] WITH OFFSET o RETURN x, o");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let for_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::For(_)));
            if let Some(SimpleQueryStatement::For(f)) = for_part {
                assert_eq!(f.variable, "x");
                let ord = f.ordinality.as_ref().expect("expected ordinality");
                assert!(ord.offset_keyword);
                assert_eq!(ord.variable, "o");
            } else {
                panic!("expected SimpleQueryStatement::For in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }
}

// ── forItem ────────────────────────────────────────────────────────────
//   : forItemAlias forItemSource
//   ;
mod for_item {
    use super::*;

    /// forItemAlias + forItemSource — variable IN list expression
    #[test]
    fn alias_and_source() {
        let prog = p("FOR elem IN [10, 20] RETURN elem");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let for_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::For(_)));
            if let Some(SimpleQueryStatement::For(f)) = for_part {
                assert_eq!(f.variable, "elem");
            } else {
                panic!("expected For");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── forOrdinalityOrOffset ──────────────────────────────────────────────
//   : WITH (ORDINALITY | OFFSET) bindingVariable
//   ;
mod for_ordinality_or_offset {
    use super::*;

    /// WITH ORDINALITY var
    #[test]
    fn ordinality_binding() {
        let prog = p("FOR x IN [1] WITH ORDINALITY idx RETURN x, idx");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let for_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::For(_)));
            if let Some(SimpleQueryStatement::For(f)) = for_part {
                let ord = f.ordinality.as_ref().expect("expected ordinality");
                assert!(!ord.offset_keyword);
                assert_eq!(ord.variable, "idx");
            } else {
                panic!("expected For");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// WITH OFFSET var
    #[test]
    fn offset_binding() {
        let prog = p("FOR x IN [1] WITH OFFSET off RETURN x, off");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let for_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::For(_)));
            if let Some(SimpleQueryStatement::For(f)) = for_part {
                let ord = f.ordinality.as_ref().expect("expected ordinality");
                assert!(ord.offset_keyword);
                assert_eq!(ord.variable, "off");
            } else {
                panic!("expected For");
            }
        } else {
            panic!("expected Query");
        }
    }
}
