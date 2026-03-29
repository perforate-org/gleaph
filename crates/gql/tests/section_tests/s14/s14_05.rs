//! §14.5 — Call query statement.
//!
//! GQL rules: callQueryStatement, callProcedureStatement, procedureCall.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── callQueryStatement ───────────────────────────────────────────────────
//   : callProcedureStatement
//   ;
// ── callProcedureStatement ───────────────────────────────────────────────
//   : OPTIONAL? CALL procedureCall
//   ;
// ── procedureCall ────────────────────────────────────────────────────────
//   : inlineProcedureCall | namedProcedureCall
//   ;
mod call_query_statement {
    use super::*;

    fn extract_call(input: &str) -> CallProcedureStatement {
        let prog = p(input);
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            for part in &q.left.parts {
                if let SimpleQueryStatement::CallProcedure(c) = part {
                    return c.clone();
                }
            }
            panic!("no CallProcedure found in parts: {:?}", q.left.parts);
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// CALL myProc() YIELD x RETURN x — named call in query context.
    #[test]
    fn named_call() {
        let c = extract_call("CALL myProc() YIELD x RETURN x");
        assert!(!c.optional, "expected optional=false");
        assert!(!c.name.parts.is_empty());
        assert!(c.yield_items.is_some(), "expected yield_items");
    }

    /// OPTIONAL CALL myProc() YIELD x RETURN x — optional=true.
    #[test]
    fn optional_call() {
        let c = extract_call("OPTIONAL CALL myProc() YIELD x RETURN x");
        assert!(c.optional, "expected optional=true");
    }

    /// CALL with arguments: CALL myProc(1, 'a') YIELD x RETURN x.
    #[test]
    fn call_with_args() {
        let c = extract_call("CALL myProc(1, 'a') YIELD x RETURN x");
        assert_eq!(c.args.len(), 2, "expected 2 arguments");
    }

    /// CALL yield items: CALL myProc() YIELD a, b RETURN a, b.
    #[test]
    fn call_yield_multiple() {
        let c = extract_call("CALL myProc() YIELD a, b RETURN a, b");
        let items = c.yield_items.as_ref().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "a");
        assert_eq!(items[1].name, "b");
    }
}
