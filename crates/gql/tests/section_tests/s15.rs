//! §15 — Call procedure.
//!
//! GQL rules: callProcedureStatement, procedureCall, inlineProcedureCall,
//! variableScopeClause, namedProcedureCall, procedureArgumentList.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── callProcedureStatement ───────────────────────────────────────────────
//   : OPTIONAL? CALL procedureCall
//   ;
mod call_procedure_statement {
    use super::*;

    /// CALL myProc() YIELD x RETURN x — optional=false
    #[test]
    fn non_optional() {
        let prog = p("CALL myProc() YIELD x RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
            if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                assert!(!c.optional);
            } else {
                panic!("expected SimpleQueryStatement::CallProcedure in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// OPTIONAL CALL myProc() YIELD x RETURN x — optional=true
    #[test]
    fn optional() {
        let prog = p("OPTIONAL CALL myProc() YIELD x RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
            if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                assert!(c.optional);
            } else {
                panic!("expected SimpleQueryStatement::CallProcedure in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }
}

// ── procedureCall ────────────────────────────────────────────────────────
//   : inlineProcedureCall | namedProcedureCall
//   ;
mod procedure_call {
    use super::*;

    /// Inline variant: CALL { MATCH (n) RETURN n } — InlineProcedureCall
    #[test]
    fn inline() {
        let prog = p("CALL { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::InlineProcedureCall(_)));
            assert!(call.is_some(), "expected InlineProcedureCall in parts");
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// Named variant: CALL myProc() YIELD x RETURN x — CallProcedure
    #[test]
    fn named() {
        let prog = p("CALL myProc() YIELD x RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
            assert!(call.is_some(), "expected CallProcedure in parts");
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }
}

// ── inlineProcedureCall ──────────────────────────────────────────────────
//   : variableScopeClause? nestedProcedureSpecification
//   ;
mod inline_procedure_call {
    use super::*;

    /// CALL { MATCH (n) RETURN n } — implicit all scope
    #[test]
    fn no_scope_vars() {
        let prog = p("CALL { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::InlineProcedureCall(_)));
            if let Some(SimpleQueryStatement::InlineProcedureCall(ic)) = call {
                assert!(matches!(ic.scope, InlineProcedureScope::ImplicitAll));
                assert!(!ic.optional);
            } else {
                panic!("expected InlineProcedureCall in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// CALL (a, b) { MATCH (n) RETURN n } — scope_vars = ["a", "b"]
    #[test]
    fn with_scope_vars() {
        let prog = p("CALL (a, b) { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::InlineProcedureCall(_)));
            if let Some(SimpleQueryStatement::InlineProcedureCall(ic)) = call {
                assert_eq!(
                    ic.scope,
                    InlineProcedureScope::Explicit(vec!["a".to_string(), "b".to_string()])
                );
            } else {
                panic!("expected InlineProcedureCall in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }
}

// ── variableScopeClause ──────────────────────────────────────────────────
//   : LEFT_PAREN bindingVariableReferenceList? RIGHT_PAREN
//   ;
mod variable_scope_clause {
    use super::*;

    /// CALL () { MATCH (n) RETURN n } — explicit empty scope
    #[test]
    fn empty() {
        let prog = p("CALL () { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::InlineProcedureCall(_)));
            if let Some(SimpleQueryStatement::InlineProcedureCall(ic)) = call {
                assert_eq!(ic.scope, InlineProcedureScope::Explicit(vec![]));
            } else {
                panic!("expected InlineProcedureCall in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// CALL (x) { MATCH (n) RETURN n } — scope_vars = ["x"]
    #[test]
    fn with_vars() {
        let prog = p("CALL (x) { MATCH (n) RETURN n }");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::InlineProcedureCall(_)));
            if let Some(SimpleQueryStatement::InlineProcedureCall(ic)) = call {
                assert_eq!(
                    ic.scope,
                    InlineProcedureScope::Explicit(vec!["x".to_string()])
                );
            } else {
                panic!("expected InlineProcedureCall in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }
}

// ── namedProcedureCall ───────────────────────────────────────────────────
//   : procedureReference LEFT_PAREN procedureArgumentList? RIGHT_PAREN yieldClause?
//   ;
mod named_procedure_call {
    use super::*;

    /// CALL myProc() YIELD x RETURN x — no args
    #[test]
    fn no_args() {
        let prog = p("CALL myProc() YIELD x RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
            if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                assert_eq!(c.name, ObjectName::simple("myProc"));
                assert!(c.args.is_empty());
            } else {
                panic!("expected CallProcedure in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// CALL myProc(1, 'hello') YIELD x RETURN x — args=[Int, String]
    #[test]
    fn with_args() {
        let prog = p("CALL myProc(1, 'hello') YIELD x RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
            if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                assert_eq!(c.args.len(), 2);
                assert_eq!(c.args[0], Expr::int(1));
                assert_eq!(c.args[1], Expr::string("hello"));
            } else {
                panic!("expected CallProcedure in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// CALL myProc() YIELD x RETURN x — yield_items present
    #[test]
    fn with_yield() {
        let prog = p("CALL myProc() YIELD x RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
            if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                let items = c.yield_items.as_ref().expect("expected yield_items");
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].name, "x");
                assert_eq!(items[0].alias, None);
            } else {
                panic!("expected CallProcedure in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// CALL myProc() YIELD a AS b RETURN b — yield with alias
    #[test]
    fn yield_with_alias() {
        let prog = p("CALL myProc() YIELD a AS b RETURN b");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let call = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
            if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                let items = c.yield_items.as_ref().expect("expected yield_items");
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].name, "a");
                assert_eq!(items[0].alias, Some("b".to_string()));
            } else {
                panic!("expected CallProcedure in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }
}
