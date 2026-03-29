//! §10 — Variable definitions.
//!
//! GQL rules: graphVariableDefinition, optTypedGraphInitializer,
//! graphInitializer, bindingTableVariableDefinition,
//! optTypedBindingTableInitializer, bindingTableInitializer,
//! valueVariableDefinition, optTypedValueInitializer, valueInitializer.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

/// Helper: extract prefix_bindings from a parsed program.
fn bindings(input: &str) -> Vec<ProcedureBindingDefinition> {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Query(q) => q.left.prefix_bindings.clone(),
        other => panic!("expected Statement::Query, got {other:?}"),
    }
}

// ── graphVariableDefinition ─────────────────────────────────────────────
//   : PROPERTY? GRAPH bindingVariable optTypedGraphInitializer
//   ;
mod graph_variable_definition {
    use super::*;

    /// GRAPH bindingVariable = graphExpression
    #[test]
    fn graph_simple() {
        let bs = bindings("GRAPH g = myGraph MATCH (n) RETURN n");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].kind, ProcedureBindingKind::Graph);
        assert_eq!(bs[0].variable, "g");
        assert!(bs[0].type_annotation.is_none());
        assert!(
            matches!(&bs[0].initializer, ProcedureBindingInitializer::Object(name) if name.parts == ["myGraph"])
        );
    }

    /// PROPERTY GRAPH bindingVariable = graphExpression
    #[test]
    fn property_graph() {
        let bs = bindings("PROPERTY GRAPH g = myGraph MATCH (n) RETURN n");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].kind, ProcedureBindingKind::Graph);
        assert_eq!(bs[0].variable, "g");
    }
}

// ── optTypedGraphInitializer ────────────────────────────────────────────
//   : (typed? graphReferenceValueType)? graphInitializer
//   ;
mod opt_typed_graph_initializer {
    use super::*;

    /// No type annotation: GRAPH g = myGraph
    #[test]
    fn untyped() {
        let bs = bindings("GRAPH g = myGraph MATCH (n) RETURN n");
        assert!(bs[0].type_annotation.is_none());
    }

    /// With type annotation: GRAPH g :: ANY GRAPH = myGraph
    #[test]
    fn typed_any_graph() {
        let bs = bindings("GRAPH g :: ANY GRAPH = myGraph MATCH (n) RETURN n");
        assert!(matches!(
            &bs[0].type_annotation,
            Some(BindingTypeAnnotation::AnyGraph {
                not_null: false,
                ..
            })
        ));
    }
}

// ── graphInitializer ────────────────────────────────────────────────────
//   : EQUALS_OPERATOR graphExpression
//   ;
mod graph_initializer {
    use super::*;

    /// = <object name>
    #[test]
    fn object_name() {
        let bs = bindings("GRAPH g = myGraph MATCH (n) RETURN n");
        assert!(matches!(
            &bs[0].initializer,
            ProcedureBindingInitializer::Object(_)
        ));
    }

    /// = <catalog path>
    #[test]
    fn catalog_path() {
        let bs = bindings("GRAPH g = /db/myGraph MATCH (n) RETURN n");
        assert!(
            matches!(&bs[0].initializer, ProcedureBindingInitializer::Object(name) if name.parts.len() == 2)
        );
    }
}

// ── bindingTableVariableDefinition ──────────────────────────────────────
//   : BINDING? TABLE bindingVariable optTypedBindingTableInitializer
//   ;
mod binding_table_variable_definition {
    use super::*;

    /// BINDING TABLE bindingVariable = bindingTableExpression
    #[test]
    fn binding_table() {
        let bs = bindings("BINDING TABLE t = $$other MATCH (n) RETURN n");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].kind, ProcedureBindingKind::Table);
        assert_eq!(bs[0].variable, "t");
    }

    /// TABLE bindingVariable = bindingTableExpression  (BINDING is optional)
    #[test]
    fn table_without_binding() {
        let bs = bindings("TABLE t = $$other MATCH (n) RETURN n");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].kind, ProcedureBindingKind::Table);
        assert_eq!(bs[0].variable, "t");
    }
}

// ── optTypedBindingTableInitializer ─────────────────────────────────────
//   : (typed? bindingTableReferenceValueType)? bindingTableInitializer
//   ;
mod opt_typed_binding_table_initializer {
    use super::*;

    /// No type annotation
    #[test]
    fn untyped() {
        let bs = bindings("TABLE t = myTable MATCH (n) RETURN n");
        assert!(bs[0].type_annotation.is_none());
    }
}

// ── bindingTableInitializer ─────────────────────────────────────────────
//   : EQUALS_OPERATOR bindingTableExpression
//   ;
mod binding_table_initializer {
    use super::*;

    /// = <object name>
    #[test]
    fn object_name() {
        let bs = bindings("TABLE t = myTable MATCH (n) RETURN n");
        assert!(matches!(
            &bs[0].initializer,
            ProcedureBindingInitializer::Object(_)
        ));
    }

    /// = { nested query }
    #[test]
    fn nested_query() {
        let bs = bindings("TABLE t = { MATCH (n) RETURN n } MATCH (m) RETURN m");
        assert!(matches!(
            &bs[0].initializer,
            ProcedureBindingInitializer::Query(_)
        ));
    }
}

// ── valueVariableDefinition ─────────────────────────────────────────────
//   : VALUE bindingVariable optTypedValueInitializer
//   ;
mod value_variable_definition {
    use super::*;

    /// VALUE bindingVariable = valueExpression
    #[test]
    fn value_simple() {
        let bs = bindings("VALUE x = 42 MATCH (n) RETURN n");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].kind, ProcedureBindingKind::Value);
        assert_eq!(bs[0].variable, "x");
        assert!(bs[0].type_annotation.is_none());
        assert!(matches!(
            &bs[0].initializer,
            ProcedureBindingInitializer::Expr(_)
        ));
    }
}

// ── optTypedValueInitializer ────────────────────────────────────────────
//   : (typed? valueType)? valueInitializer
//   ;
mod opt_typed_value_initializer {
    use super::*;

    /// No type annotation: VALUE x = 42
    #[test]
    fn untyped() {
        let bs = bindings("VALUE x = 42 MATCH (n) RETURN n");
        assert!(bs[0].type_annotation.is_none());
    }

    /// With type annotation: VALUE x :: INT32 = 42
    #[test]
    fn typed_int32() {
        let bs = bindings("VALUE x :: INT32 = 42 MATCH (n) RETURN n");
        assert!(bs[0].type_annotation.is_some());
        assert!(matches!(
            &bs[0].type_annotation,
            Some(BindingTypeAnnotation::Value(ValueType::Int32 { .. }))
        ));
    }

    /// With TYPED keyword: VALUE x TYPED STRING = 'hello'
    #[test]
    fn typed_keyword_string() {
        let bs = bindings("VALUE x TYPED STRING = 'hello' MATCH (n) RETURN n");
        assert!(matches!(
            &bs[0].type_annotation,
            Some(BindingTypeAnnotation::Value(ValueType::String { .. }))
        ));
    }
}

// ── valueInitializer ────────────────────────────────────────────────────
//   : EQUALS_OPERATOR valueExpression
//   ;
mod value_initializer {
    use super::*;

    /// = <literal>
    #[test]
    fn literal_expr() {
        let bs = bindings("VALUE x = 42 MATCH (n) RETURN n");
        assert!(matches!(
            &bs[0].initializer,
            ProcedureBindingInitializer::Expr(_)
        ));
    }

    /// = <string literal>
    #[test]
    fn string_literal() {
        let bs = bindings("VALUE x = 'hello' MATCH (n) RETURN n");
        assert!(matches!(
            &bs[0].initializer,
            ProcedureBindingInitializer::Expr(_)
        ));
    }
}

// ── multiple bindings ───────────────────────────────────────────────────
mod multiple_bindings {
    use super::*;

    /// Multiple bindings of different kinds
    #[test]
    fn mixed_kinds() {
        let bs = bindings("GRAPH g = myGraph VALUE x = 1 TABLE t = myTable MATCH (n) RETURN n");
        assert_eq!(bs.len(), 3);
        assert_eq!(bs[0].kind, ProcedureBindingKind::Graph);
        assert_eq!(bs[1].kind, ProcedureBindingKind::Value);
        assert_eq!(bs[2].kind, ProcedureBindingKind::Table);
    }

    /// Multiple value bindings
    #[test]
    fn multiple_values() {
        let bs = bindings("VALUE x = 1 VALUE y = 2 MATCH (n) RETURN n");
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].variable, "x");
        assert_eq!(bs[1].variable, "y");
    }
}
