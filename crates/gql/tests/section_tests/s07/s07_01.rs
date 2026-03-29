//! §7.1 — Session set command.
//!
//! GQL rules: sessionSetCommand, sessionSetSchemaClause,
//! sessionSetGraphClause, sessionSetTimeZoneClause, sessionSetParameterClause,
//! sessionSetGraphParameterClause, sessionSetBindingTableParameterClause,
//! sessionSetValueParameterClause, sessionSetParameterName, setTimeZoneValue.

use crate::section_tests::p;
use gleaph_gql::Value;
use gleaph_gql::ast::*;

// ── sessionSetCommand ────────────────────────────────────────────────────
//   : SESSION SET (sessionSetSchemaClause | sessionSetGraphClause
//                 | sessionSetTimeZoneClause | sessionSetParameterClause)
//   ;
mod session_set_command {
    use super::*;

    /// sessionSetSchemaClause
    #[test]
    fn schema_clause() {
        let prog = p("SESSION SET SCHEMA /mydb");
        assert_eq!(prog.session_activity.len(), 1);
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::Schema(_))
        ));
    }

    /// sessionSetGraphClause
    #[test]
    fn graph_clause() {
        let prog = p("SESSION SET GRAPH myGraph");
        assert_eq!(prog.session_activity.len(), 1);
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::Graph { .. })
        ));
    }

    /// sessionSetTimeZoneClause
    #[test]
    fn time_zone_clause() {
        let prog = p("SESSION SET TIME ZONE 'UTC'");
        assert_eq!(prog.session_activity.len(), 1);
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::TimeZone(_))
        ));
    }

    /// sessionSetParameterClause (value)
    #[test]
    fn parameter_clause_value() {
        let prog = p("SESSION SET VALUE $x = 42");
        assert_eq!(prog.session_activity.len(), 1);
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::Parameter { .. })
        ));
    }
}

// ── sessionSetSchemaClause ───────────────────────────────────────────────
//   : SCHEMA schemaReference
//   ;
mod session_set_schema_clause {
    use super::*;

    /// SCHEMA schemaReference (absolute path)
    #[test]
    fn schema_absolute() {
        let prog = p("SESSION SET SCHEMA /mydb");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Schema(name)) => {
                assert!(!name.parts.is_empty());
            }
            other => panic!("expected Schema, got {other:?}"),
        }
    }
}

// ── sessionSetGraphClause ────────────────────────────────────────────────
//   : PROPERTY? GRAPH graphExpression
//   ;
mod session_set_graph_clause {
    use super::*;

    /// GRAPH graphExpression
    #[test]
    fn graph() {
        let prog = p("SESSION SET GRAPH myGraph");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::Graph { .. })
        ));
    }

    /// PROPERTY GRAPH graphExpression
    #[test]
    fn property_graph() {
        let prog = p("SESSION SET PROPERTY GRAPH myGraph");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::Graph { .. })
        ));
    }
}

// ── sessionSetTimeZoneClause ─────────────────────────────────────────────
//   : TIME ZONE setTimeZoneValue
//   ;
// ── setTimeZoneValue ─────────────────────────────────────────────────────
//   : timeZoneString
//   ;
mod session_set_time_zone_clause {
    use super::*;

    /// TIME ZONE 'string'
    #[test]
    fn time_zone_string() {
        let prog = p("SESSION SET TIME ZONE 'America/New_York'");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::TimeZone(expr)) => {
                assert!(matches!(
                    expr.as_ref().kind,
                    ExprKind::Literal(Value::Text(_))
                ));
            }
            other => panic!("expected TimeZone, got {other:?}"),
        }
    }
}

// ── sessionSetParameterClause ────────────────────────────────────────────
//   : sessionSetGraphParameterClause
//   | sessionSetBindingTableParameterClause
//   | sessionSetValueParameterClause
//   ;
mod session_set_parameter_clause {
    use super::*;

    /// sessionSetGraphParameterClause
    #[test]
    fn graph_parameter() {
        let prog = p("SESSION SET GRAPH $g = myGraph");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::GraphParameter { .. })
        ));
    }

    /// sessionSetBindingTableParameterClause
    #[test]
    fn binding_table_parameter() {
        let prog = p("SESSION SET TABLE $t = $other");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::BindingTableParameter { .. })
        ));
    }

    /// sessionSetValueParameterClause
    #[test]
    fn value_parameter() {
        let prog = p("SESSION SET VALUE $x = 42");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Set(SessionSetCommand::Parameter { .. })
        ));
    }
}

// ── sessionSetGraphParameterClause ───────────────────────────────────────
//   : PROPERTY? GRAPH sessionSetParameterName optTypedGraphInitializer
//   ;
mod session_set_graph_parameter_clause {
    use super::*;

    /// GRAPH $name = expr
    #[test]
    fn graph_param() {
        let prog = p("SESSION SET GRAPH $g = myGraph");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::GraphParameter { name, .. }) => {
                assert_eq!(name, "g");
            }
            other => panic!("expected GraphParameter, got {other:?}"),
        }
    }

    /// PROPERTY GRAPH $name = expr
    #[test]
    fn property_graph_param() {
        let prog = p("SESSION SET PROPERTY GRAPH $g = myGraph");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::GraphParameter { name, .. }) => {
                assert_eq!(name, "g");
            }
            other => panic!("expected GraphParameter, got {other:?}"),
        }
    }
}

// ── sessionSetBindingTableParameterClause ────────────────────────────────
//   : BINDING? TABLE sessionSetParameterName optTypedBindingTableInitializer
//   ;
mod session_set_binding_table_parameter_clause {
    use super::*;

    /// TABLE $name = expr
    #[test]
    fn table_param() {
        let prog = p("SESSION SET TABLE $t = $other");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::BindingTableParameter { name, .. }) => {
                assert_eq!(name, "t");
            }
            other => panic!("expected BindingTableParameter, got {other:?}"),
        }
    }

    /// BINDING TABLE $name = expr
    #[test]
    fn binding_table_param() {
        let prog = p("SESSION SET BINDING TABLE $t = $other");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::BindingTableParameter { name, .. }) => {
                assert_eq!(name, "t");
            }
            other => panic!("expected BindingTableParameter, got {other:?}"),
        }
    }
}

// ── sessionSetValueParameterClause ───────────────────────────────────────
//   : VALUE sessionSetParameterName optTypedValueInitializer
//   ;
mod session_set_value_parameter_clause {
    use super::*;

    /// VALUE $name = expr
    #[test]
    fn value_param() {
        let prog = p("SESSION SET VALUE $x = 42");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Parameter { name, value, .. }) => {
                assert_eq!(name, "x");
                assert!(matches!(
                    value.as_ref().kind,
                    ExprKind::Literal(Value::Int64(_))
                ));
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }

    /// VALUE $name :: type = expr  (with type annotation)
    #[test]
    fn value_param_typed() {
        let prog = p("SESSION SET VALUE $x :: INT32 = 42");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Parameter {
                name,
                type_annotation,
                ..
            }) => {
                assert_eq!(name, "x");
                assert!(type_annotation.is_some());
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }
}

// ── sessionSetParameterName ──────────────────────────────────────────────
//   : (IF NOT EXISTS)? sessionParameterSpecification
//   ;
mod session_set_parameter_name {
    use super::*;

    /// sessionParameterSpecification  (bare $name)
    #[test]
    fn bare_param() {
        let prog = p("SESSION SET VALUE $x = 1");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Parameter { name, .. }) => {
                assert_eq!(name, "x");
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }

    /// IF NOT EXISTS $name = value  (value parameter)
    #[test]
    fn if_not_exists_value() {
        let prog = p("SESSION SET VALUE IF NOT EXISTS $x = 42");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Parameter {
                if_not_exists,
                name,
                ..
            }) => {
                assert!(if_not_exists);
                assert_eq!(name, "x");
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }

    /// IF NOT EXISTS $name = graph  (graph parameter)
    #[test]
    fn if_not_exists_graph() {
        let prog = p("SESSION SET GRAPH IF NOT EXISTS $g = myGraph");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::GraphParameter {
                if_not_exists,
                name,
                ..
            }) => {
                assert!(if_not_exists);
                assert_eq!(name, "g");
            }
            other => panic!("expected GraphParameter, got {other:?}"),
        }
    }

    /// IF NOT EXISTS $name = table  (binding table parameter)
    #[test]
    fn if_not_exists_table() {
        let prog = p("SESSION SET TABLE IF NOT EXISTS $t = $other");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::BindingTableParameter {
                if_not_exists,
                name,
                ..
            }) => {
                assert!(if_not_exists);
                assert_eq!(name, "t");
            }
            other => panic!("expected BindingTableParameter, got {other:?}"),
        }
    }
}
