//! §14.7 — Let statement.
//!
//! GQL rules: letStatement, letVariableDefinitionList, letVariableDefinition.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

// ── letStatement ───────────────────────────────────────────────────────
//   : LET letVariableDefinitionList
//   ;
mod let_statement {
    use super::*;

    /// LET x = n.age — single binding
    #[test]
    fn single_binding() {
        let prog = p("MATCH (n) LET x = n.age RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let let_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Let(_)));
            if let Some(SimpleQueryStatement::Let(l)) = let_part {
                assert_eq!(l.bindings.len(), 1);
                assert_eq!(l.bindings[0].variable, "x");
            } else {
                panic!("expected SimpleQueryStatement::Let in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }

    /// LET x = 1, y = 2 — multiple bindings
    #[test]
    fn multiple_bindings() {
        let prog = p("MATCH (n) LET x = 1, y = 2 RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let let_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Let(_)));
            if let Some(SimpleQueryStatement::Let(l)) = let_part {
                assert_eq!(l.bindings.len(), 2);
                assert_eq!(l.bindings[0].variable, "x");
                assert_eq!(l.bindings[0].value, Expr::int(1));
                assert_eq!(l.bindings[1].variable, "y");
                assert_eq!(l.bindings[1].value, Expr::int(2));
            } else {
                panic!("expected SimpleQueryStatement::Let in parts");
            }
        } else {
            panic!("expected Statement::Query, got {:?}", b.first);
        }
    }
}

// ── letVariableDefinitionList ──────────────────────────────────────────
//   : letVariableDefinition (COMMA letVariableDefinition)*
//   ;
mod let_variable_definition_list {
    use super::*;

    /// Single definition in the list
    #[test]
    fn single_definition() {
        let prog = p("MATCH (n) LET a = 42 RETURN a");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let let_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Let(_)));
            if let Some(SimpleQueryStatement::Let(l)) = let_part {
                assert_eq!(l.bindings.len(), 1);
                assert_eq!(l.bindings[0].variable, "a");
                assert_eq!(l.bindings[0].value, Expr::int(42));
            } else {
                panic!("expected Let");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// Multiple comma-separated definitions
    #[test]
    fn comma_separated_definitions() {
        let prog = p("MATCH (n) LET a = 1, b = 2, c = 3 RETURN a");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let let_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Let(_)));
            if let Some(SimpleQueryStatement::Let(l)) = let_part {
                assert_eq!(l.bindings.len(), 3);
                assert_eq!(l.bindings[0].variable, "a");
                assert_eq!(l.bindings[1].variable, "b");
                assert_eq!(l.bindings[2].variable, "c");
            } else {
                panic!("expected Let");
            }
        } else {
            panic!("expected Query");
        }
    }
}

// ── letVariableDefinition ──────────────────────────────────────────────
//   : valueVariableDefinition
//   | bindingVariable EQUALS_OPERATOR valueExpression
//   ;
mod let_variable_definition {
    use super::*;

    /// bindingVariable = valueExpression — variable bound to literal
    #[test]
    fn binding_to_literal() {
        let prog = p("MATCH (n) LET x = 100 RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let let_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Let(_)));
            if let Some(SimpleQueryStatement::Let(l)) = let_part {
                assert_eq!(l.bindings[0].variable, "x");
                assert_eq!(l.bindings[0].value, Expr::int(100));
            } else {
                panic!("expected Let");
            }
        } else {
            panic!("expected Query");
        }
    }

    /// bindingVariable = valueExpression — variable bound to property access
    #[test]
    fn binding_to_property_access() {
        let prog = p("MATCH (n) LET x = n.age RETURN x");
        let b = body(&prog);
        if let Statement::Query(q) = &b.first {
            let let_part = q
                .left
                .parts
                .iter()
                .find(|p| matches!(p, SimpleQueryStatement::Let(_)));
            if let Some(SimpleQueryStatement::Let(l)) = let_part {
                assert_eq!(l.bindings[0].variable, "x");
                assert!(matches!(
                    &l.bindings[0].value,
                    Expr { kind: ExprKind::PropertyAccess { property, .. }, .. } if property == "age"
                ));
            } else {
                panic!("expected Let");
            }
        } else {
            panic!("expected Query");
        }
    }
}
