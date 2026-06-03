//! §9 — Nested procedure specification / procedure body.
//!
//! GQL rules: nestedProcedureSpecification,
//! nestedDataModifyingProcedureSpecification, nestedQuerySpecification.
//!
//! These are tested via `CALL { ... }` inline procedure calls.

use super::*;
use gleaph_gql::ast::*;

/// Extract the first `InlineProcedureCall` from the body of a parsed program.
fn first_inline_call(prog: &GqlProgram) -> &InlineProcedureCall {
    let b = body(prog);
    match &b.first {
        Statement::Query(cqe) => {
            for part in &cqe.left.parts {
                if let SimpleQueryStatement::InlineProcedureCall(ipc) = part {
                    return ipc;
                }
            }
            panic!("no InlineProcedureCall found in query parts");
        }
        _ => panic!("expected Statement::Query, got {:?}", b.first),
    }
}

// ── nestedProcedureSpecification ────────────────────────────────────────
//   : LEFT_BRACE procedureSpecification RIGHT_BRACE
//   ;
mod nested_procedure_specification {
    use super::*;

    /// CALL { MATCH (n) RETURN n }
    #[test]
    fn inline_call_with_match_return() {
        let prog = p("CALL { MATCH (n) RETURN n }");
        let ipc = first_inline_call(&prog);
        assert!(!ipc.optional);
        assert!(ipc.use_graph.is_none());
        assert!(matches!(ipc.scope, InlineProcedureScope::ImplicitAll));
        // The body should contain a linear query with a MATCH part and a RETURN result.
        let linear = &ipc.body.left;
        assert!(!linear.parts.is_empty());
        assert!(matches!(linear.parts[0], SimpleQueryStatement::Match(_)));
        assert!(linear.result.is_some());
    }

    /// OPTIONAL CALL { MATCH (n) RETURN n }
    #[test]
    fn optional_inline_call() {
        let prog = p("OPTIONAL CALL { MATCH (n) RETURN n }");
        let ipc = first_inline_call(&prog);
        assert!(ipc.optional);
    }
}

// ── nestedDataModifyingProcedureSpecification ───────────────────────────
//   : LEFT_BRACE procedureBody RIGHT_BRACE
//   ;
mod nested_data_modifying_procedure_specification {
    use super::*;

    /// CALL { INSERT (:Person) }
    #[test]
    fn inline_call_with_insert() {
        let prog = p("CALL { INSERT (:Person) }");
        let ipc = first_inline_call(&prog);
        assert!(!ipc.optional);
        assert!(ipc.use_graph.is_none());
        // The body should contain an INSERT as a query part.
        let linear = &ipc.body.left;
        assert!(!linear.parts.is_empty());
        assert!(matches!(linear.parts[0], SimpleQueryStatement::Insert(_)));
    }
}

// ── nestedQuerySpecification ────────────────────────────────────────────
//   : LEFT_BRACE procedureBody RIGHT_BRACE
//   ;
mod nested_query_specification {
    use super::*;

    /// CALL { MATCH (a)-[e]->(b) RETURN a, b } — query-only nested body
    #[test]
    fn inline_call_query_body() {
        let prog = p("CALL { MATCH (a)-[e]->(b) RETURN a, b }");
        let ipc = first_inline_call(&prog);
        assert!(!ipc.optional);
        let linear = &ipc.body.left;
        assert!(matches!(linear.parts[0], SimpleQueryStatement::Match(_)));
        assert!(linear.result.is_some());
    }

    /// Inline call with scope clause: CALL (x) { MATCH (n) RETURN n }
    #[test]
    fn inline_call_with_scope_vars() {
        let prog = p("MATCH (x) CALL (x) { MATCH (n) WHERE n.id = x.id RETURN n }");
        let ipc = first_inline_call(&prog);
        assert_eq!(
            ipc.scope,
            InlineProcedureScope::Explicit(vec!["x".to_string()])
        );
    }
}
