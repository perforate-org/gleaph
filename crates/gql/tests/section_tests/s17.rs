//! §17 — References (schema, graph, graph type, binding table, procedure).
//!
//! GQL rules: schemaReference, graphReference, graphTypeReference,
//! bindingTableReference, procedureReference.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── schemaReference ─────────────────────────────────────────────────────
//   : absoluteCatalogSchemaReference
//   | relativeCatalogSchemaReference
//   | referenceParameter
//   | CURRENT_SCHEMA | HOME_SCHEMA
//   ;
mod schema_reference {
    use super::*;

    /// AT /mydb MATCH (n) RETURN n — absolute catalog path
    #[test]
    fn absolute_path() {
        let prog = p("AT /mydb MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let at = cq.left.at_schema.as_ref().expect("expected at_schema");
                match at {
                    SchemaReference::Absolute(parts) => {
                        assert!(parts.contains(&"mydb".to_string()));
                    }
                    other => panic!("expected Absolute, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// AT CURRENT_SCHEMA MATCH (n) RETURN n — current schema keyword
    #[test]
    fn current_schema() {
        let prog = p("AT CURRENT_SCHEMA MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let at = cq.left.at_schema.as_ref().expect("expected at_schema");
                assert!(
                    matches!(at, SchemaReference::Current(s) if s == "CURRENT_SCHEMA"),
                    "expected Current(\"CURRENT_SCHEMA\"), got {at:?}"
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// AT HOME_SCHEMA MATCH (n) RETURN n — home schema keyword
    #[test]
    fn home_schema() {
        let prog = p("AT HOME_SCHEMA MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let at = cq.left.at_schema.as_ref().expect("expected at_schema");
                assert!(
                    matches!(at, SchemaReference::Current(s) if s == "HOME_SCHEMA"),
                    "expected Current(\"HOME_SCHEMA\"), got {at:?}"
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}

// ── graphReference ──────────────────────────────────────────────────────
//   : catalogObjectParentReference? graphName
//   | catalogObjectParentReference? substitutedParameterReference
//   ;
mod graph_reference {
    use super::*;

    /// USE schema1.myGraph MATCH (n) RETURN n — qualified graph reference
    #[test]
    fn qualified() {
        let prog = p("USE schema1.myGraph MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let focused = cq
                    .left
                    .parts
                    .iter()
                    .find(|p| matches!(p, SimpleQueryStatement::Focused { .. }));
                if let Some(SimpleQueryStatement::Focused { graph, .. }) = focused {
                    assert_eq!(graph.parts.len(), 2);
                    assert_eq!(graph.parts[0], "schema1");
                    assert_eq!(graph.parts[1], "myGraph");
                } else {
                    panic!("expected Focused in parts");
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// USE /catalog/myGraph MATCH (n) RETURN n — absolute catalog path
    #[test]
    fn absolute() {
        let prog = p("USE /catalog/myGraph MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let focused = cq
                    .left
                    .parts
                    .iter()
                    .find(|p| matches!(p, SimpleQueryStatement::Focused { .. }));
                if let Some(SimpleQueryStatement::Focused { graph, .. }) = focused {
                    assert!(
                        graph.parts.len() >= 2,
                        "expected at least 2 parts in absolute path, got {:?}",
                        graph.parts
                    );
                } else {
                    panic!("expected Focused in parts");
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}

// ── procedureReference ──────────────────────────────────────────────────
//   : catalogObjectParentReference? procedureName
//   ;
mod procedure_reference {
    use super::*;

    /// CALL schema1.myProc() YIELD x RETURN x — qualified procedure name
    #[test]
    fn qualified() {
        let prog = p("CALL schema1.myProc() YIELD x RETURN x");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let call = cq
                    .left
                    .parts
                    .iter()
                    .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
                if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                    assert_eq!(
                        c.name.parts.len(),
                        2,
                        "expected 2 parts, got {:?}",
                        c.name.parts
                    );
                    assert_eq!(c.name.parts[0], "schema1");
                    assert_eq!(c.name.parts[1], "myProc");
                } else {
                    panic!("expected CallProcedure in parts");
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// CALL myProc() YIELD x RETURN x — simple (unqualified) procedure name
    #[test]
    fn simple() {
        let prog = p("CALL myProc() YIELD x RETURN x");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let call = cq
                    .left
                    .parts
                    .iter()
                    .find(|p| matches!(p, SimpleQueryStatement::CallProcedure(_)));
                if let Some(SimpleQueryStatement::CallProcedure(c)) = call {
                    assert_eq!(c.name.parts.len(), 1);
                    assert_eq!(c.name.parts[0], "myProc");
                } else {
                    panic!("expected CallProcedure in parts");
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
