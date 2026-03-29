//! §16.1 — AT schema clause.
//!
//! GQL rule: `atSchemaClause : AT schemaReference`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── atSchemaClause ──────────────────────────────────────────────────────
//   : AT schemaReference
//   ;
mod at_schema_clause {
    use super::*;

    /// AT /mydb MATCH (n) RETURN n — at_schema is Absolute
    #[test]
    fn at_absolute_path() {
        let prog = p("AT /mydb MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let schema = cq
                    .left
                    .at_schema
                    .as_ref()
                    .expect("expected at_schema to be Some");
                match schema {
                    SchemaReference::Absolute(parts) => {
                        assert!(
                            parts.contains(&"mydb".to_string()),
                            "expected 'mydb' in path, got {parts:?}"
                        );
                    }
                    other => panic!("expected SchemaReference::Absolute, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// AT CURRENT_SCHEMA MATCH (n) RETURN n — at_schema is Current
    #[test]
    fn at_current_schema() {
        let prog = p("AT CURRENT_SCHEMA MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let schema = cq
                    .left
                    .at_schema
                    .as_ref()
                    .expect("expected at_schema to be Some");
                match schema {
                    SchemaReference::Current(kw) => {
                        assert_eq!(kw.to_uppercase(), "CURRENT_SCHEMA");
                    }
                    other => panic!("expected SchemaReference::Current, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// AT HOME_SCHEMA MATCH (n) RETURN n — at_schema is Current("HOME_SCHEMA")
    #[test]
    fn at_home_schema() {
        let prog = p("AT HOME_SCHEMA MATCH (n) RETURN n");
        let b = body(&prog);
        match &b.first {
            Statement::Query(cq) => {
                let schema = cq
                    .left
                    .at_schema
                    .as_ref()
                    .expect("expected at_schema to be Some");
                match schema {
                    SchemaReference::Current(kw) => {
                        assert_eq!(kw.to_uppercase(), "HOME_SCHEMA");
                    }
                    other => panic!("expected SchemaReference::Current, got {other:?}"),
                }
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
