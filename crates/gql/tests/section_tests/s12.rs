//! §12 — DDL statements.
//!
//! GQL rules: linearCatalogModifyingStatement,
//! simpleCatalogModifyingStatement, primitiveCatalogModifyingStatement,
//! createSchemaStatement, dropSchemaStatement, createGraphStatement,
//! dropGraphStatement, createGraphTypeStatement, dropGraphTypeStatement.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── 12.1 linearCatalogModifyingStatement ────────────────────────────────
//   : simpleCatalogModifyingStatement+
//   ;
mod linear_catalog_modifying_statement {
    use super::*;

    /// Single simpleCatalogModifyingStatement
    #[test]
    fn single() {
        let prog = p("CREATE SCHEMA /s");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::CreateSchema(_)));
    }
}

// ── 12.1 simpleCatalogModifyingStatement ────────────────────────────────
//   : primitiveCatalogModifyingStatement
//   | callCatalogModifyingProcedureStatement
//   ;
mod simple_catalog_modifying_statement {
    use super::*;

    /// primitiveCatalogModifyingStatement — CREATE SCHEMA
    #[test]
    fn primitive_create_schema() {
        let prog = p("CREATE SCHEMA /s");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::CreateSchema(_)));
    }

    /// primitiveCatalogModifyingStatement — DROP GRAPH
    #[test]
    fn primitive_drop_graph() {
        let prog = p("DROP GRAPH g");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::DropGraph(_)));
    }
}

// ── 12.1 primitiveCatalogModifyingStatement ─────────────────────────────
//   : createSchemaStatement | dropSchemaStatement
//   | createGraphStatement  | dropGraphStatement
//   | createGraphTypeStatement | dropGraphTypeStatement
//   ;
mod primitive_catalog_modifying_statement {
    use super::*;

    #[test]
    fn create_schema() {
        let prog = p("CREATE SCHEMA /s");
        assert!(matches!(body(&prog).first, Statement::CreateSchema(_)));
    }

    #[test]
    fn drop_schema() {
        let prog = p("DROP SCHEMA /s");
        assert!(matches!(body(&prog).first, Statement::DropSchema(_)));
    }

    #[test]
    fn create_graph() {
        let prog = p("CREATE GRAPH g ANY");
        assert!(matches!(body(&prog).first, Statement::CreateGraph(_)));
    }

    #[test]
    fn drop_graph() {
        let prog = p("DROP GRAPH g");
        assert!(matches!(body(&prog).first, Statement::DropGraph(_)));
    }

    #[test]
    fn create_graph_type() {
        let prog = p("CREATE GRAPH TYPE gt {}");
        assert!(matches!(body(&prog).first, Statement::CreateGraphType(_)));
    }

    #[test]
    fn drop_graph_type() {
        let prog = p("DROP GRAPH TYPE gt");
        assert!(matches!(body(&prog).first, Statement::DropGraphType(_)));
    }
}

// ── 12.2 createSchemaStatement ──────────────────────────────────────────
//   : CREATE SCHEMA (IF NOT EXISTS)? catalogSchemaParentAndName
//   ;
mod create_schema_statement {
    use super::*;

    /// CREATE SCHEMA /s — no IF NOT EXISTS
    #[test]
    fn basic() {
        let prog = p("CREATE SCHEMA /s");
        let Statement::CreateSchema(ref cs) = body(&prog).first else {
            panic!("expected CreateSchema");
        };
        assert!(!cs.if_not_exists);
    }

    /// CREATE SCHEMA IF NOT EXISTS /s
    #[test]
    fn if_not_exists() {
        let prog = p("CREATE SCHEMA IF NOT EXISTS /s");
        let Statement::CreateSchema(ref cs) = body(&prog).first else {
            panic!("expected CreateSchema");
        };
        assert!(cs.if_not_exists);
    }
}

// ── 12.3 dropSchemaStatement ────────────────────────────────────────────
//   : DROP SCHEMA (IF EXISTS)? catalogSchemaParentAndName
//   ;
mod drop_schema_statement {
    use super::*;

    /// DROP SCHEMA /s — no IF EXISTS
    #[test]
    fn basic() {
        let prog = p("DROP SCHEMA /s");
        let Statement::DropSchema(ref ds) = body(&prog).first else {
            panic!("expected DropSchema");
        };
        assert!(!ds.if_exists);
    }

    /// DROP SCHEMA IF EXISTS /s
    #[test]
    fn if_exists() {
        let prog = p("DROP SCHEMA IF EXISTS /s");
        let Statement::DropSchema(ref ds) = body(&prog).first else {
            panic!("expected DropSchema");
        };
        assert!(ds.if_exists);
    }
}

// ── 12.4 createGraphStatement ───────────────────────────────────────────
//   : CREATE (PROPERTY? GRAPH (IF NOT EXISTS)? | OR REPLACE PROPERTY? GRAPH)
//     catalogGraphParentAndName (openGraphType | ofGraphType) graphSource?
//   ;
mod create_graph_statement {
    use super::*;

    /// CREATE GRAPH g ANY — open graph type
    #[test]
    fn open_any() {
        let prog = p("CREATE GRAPH g ANY");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(!cg.or_replace);
        assert!(!cg.if_not_exists);
        assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Any { .. })));
        assert!(cg.copy_of.is_none());
    }

    /// CREATE GRAPH g {} — inline graph type
    #[test]
    fn inline_type() {
        let prog = p("CREATE GRAPH g {}");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Inline(_))));
    }

    /// CREATE GRAPH g TYPED myType — typed reference
    #[test]
    fn typed_reference() {
        let prog = p("CREATE GRAPH g TYPED myType");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Typed { .. })));
    }

    /// CREATE GRAPH g :: myType — typed reference with double-colon
    #[test]
    fn typed_double_colon() {
        let prog = p("CREATE GRAPH g :: myType");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Typed { .. })));
    }

    /// CREATE GRAPH g LIKE other — like graph type
    #[test]
    fn like_graph() {
        let prog = p("CREATE GRAPH g LIKE other");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Like(_))));
    }

    /// CREATE GRAPH g {} AS COPY OF other — graph source
    #[test]
    fn with_copy_of() {
        let prog = p("CREATE GRAPH g {} AS COPY OF other");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(matches!(cg.graph_type, Some(GraphTypeSpec::Inline(_))));
        assert!(cg.copy_of.is_some());
    }

    /// CREATE GRAPH IF NOT EXISTS g ANY
    #[test]
    fn if_not_exists() {
        let prog = p("CREATE GRAPH IF NOT EXISTS g ANY");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(cg.if_not_exists);
        assert!(!cg.or_replace);
    }

    /// CREATE OR REPLACE GRAPH g ANY
    #[test]
    fn or_replace() {
        let prog = p("CREATE OR REPLACE GRAPH g ANY");
        let Statement::CreateGraph(ref cg) = body(&prog).first else {
            panic!("expected CreateGraph");
        };
        assert!(cg.or_replace);
        assert!(!cg.if_not_exists);
    }
}

// ── 12.5 dropGraphStatement ─────────────────────────────────────────────
//   : DROP PROPERTY? GRAPH (IF EXISTS)? catalogGraphParentAndName
//   ;
mod drop_graph_statement {
    use super::*;

    /// DROP GRAPH g — no IF EXISTS
    #[test]
    fn basic() {
        let prog = p("DROP GRAPH g");
        let Statement::DropGraph(ref dg) = body(&prog).first else {
            panic!("expected DropGraph");
        };
        assert!(!dg.if_exists);
    }

    /// DROP GRAPH IF EXISTS g
    #[test]
    fn if_exists() {
        let prog = p("DROP GRAPH IF EXISTS g");
        let Statement::DropGraph(ref dg) = body(&prog).first else {
            panic!("expected DropGraph");
        };
        assert!(dg.if_exists);
    }
}

// ── 12.6 createGraphTypeStatement ───────────────────────────────────────
//   : CREATE (PROPERTY? GRAPH TYPE (IF NOT EXISTS)? | OR REPLACE PROPERTY? GRAPH TYPE)
//     catalogGraphTypeParentAndName graphTypeSource
//   ;
mod create_graph_type_statement {
    use super::*;

    /// CREATE GRAPH TYPE gt {} — basic inline definition
    #[test]
    fn basic() {
        let prog = p("CREATE GRAPH TYPE gt {}");
        let Statement::CreateGraphType(ref cgt) = body(&prog).first else {
            panic!("expected CreateGraphType");
        };
        assert!(!cgt.or_replace);
        assert!(!cgt.if_not_exists);
        assert!(cgt.copy_of.is_none());
    }

    /// CREATE GRAPH TYPE IF NOT EXISTS gt {}
    #[test]
    fn if_not_exists() {
        let prog = p("CREATE GRAPH TYPE IF NOT EXISTS gt {}");
        let Statement::CreateGraphType(ref cgt) = body(&prog).first else {
            panic!("expected CreateGraphType");
        };
        assert!(cgt.if_not_exists);
        assert!(!cgt.or_replace);
    }

    /// CREATE OR REPLACE GRAPH TYPE gt {}
    #[test]
    fn or_replace() {
        let prog = p("CREATE OR REPLACE GRAPH TYPE gt {}");
        let Statement::CreateGraphType(ref cgt) = body(&prog).first else {
            panic!("expected CreateGraphType");
        };
        assert!(cgt.or_replace);
        assert!(!cgt.if_not_exists);
    }

    /// CREATE GRAPH TYPE gt COPY OF otherType {} — with copy_of source
    #[test]
    fn copy_of() {
        let prog = p("CREATE GRAPH TYPE gt COPY OF otherType {}");
        let Statement::CreateGraphType(ref cgt) = body(&prog).first else {
            panic!("expected CreateGraphType");
        };
        assert!(cgt.copy_of.is_some());
    }
}

// ── 12.7 dropGraphTypeStatement ─────────────────────────────────────────
//   : DROP PROPERTY? GRAPH TYPE (IF EXISTS)? catalogGraphTypeParentAndName
//   ;
mod drop_graph_type_statement {
    use super::*;

    /// DROP GRAPH TYPE gt — no IF EXISTS
    #[test]
    fn basic() {
        let prog = p("DROP GRAPH TYPE gt");
        let Statement::DropGraphType(ref dgt) = body(&prog).first else {
            panic!("expected DropGraphType");
        };
        assert!(!dgt.if_exists);
    }

    /// DROP GRAPH TYPE IF EXISTS gt
    #[test]
    fn if_exists() {
        let prog = p("DROP GRAPH TYPE IF EXISTS gt");
        let Statement::DropGraphType(ref dgt) = body(&prog).first else {
            panic!("expected DropGraphType");
        };
        assert!(dgt.if_exists);
    }
}
