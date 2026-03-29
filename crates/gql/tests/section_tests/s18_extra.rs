//! Additional type coverage tests — targeting uncovered lines in parser/types.rs.

use crate::section_tests::p;
use gleaph_gql::ast::*;

/// Helper to extract return type from CREATE GRAPH graph type spec.
fn parse_type(type_str: &str) -> ValueType {
    // Use a SESSION SET VALUE with a type annotation to parse the type
    let input = format!("SESSION SET VALUE $x :: {type_str} = 42");
    let prog = p(&input);
    match &prog.session_activity[0] {
        SessionCommand::Set(SessionSetCommand::Parameter {
            type_annotation, ..
        }) => match type_annotation.as_ref().unwrap() {
            BindingTypeAnnotation::Value(vt) => vt.clone(),
            other => panic!("expected Value, got {other:?}"),
        },
        other => panic!("expected Parameter, got {other:?}"),
    }
}

mod type_coverage {
    use super::*;

    #[test]
    fn closed_dynamic_union_with_postfix_list() {
        // Lines 41-52 — pipe union with postfix LIST
        let ty = parse_type("INT32 | STRING LIST");
        match ty {
            ValueType::ClosedDynamicUnion(members) => {
                assert_eq!(members.len(), 2);
            }
            other => panic!("expected ClosedDynamicUnion, got {other:?}"),
        }
    }

    #[test]
    fn null_not_null_type() {
        // Lines 564-568 — NULL NOT NULL
        let ty = parse_type("NULL NOT NULL");
        match ty {
            ValueType::NotNull(inner) => {
                assert_eq!(*inner, ValueType::Null);
            }
            other => panic!("expected NotNull(Null), got {other:?}"),
        }
    }

    #[test]
    fn nothing_type() {
        let ty = parse_type("NOTHING");
        assert_eq!(ty, ValueType::Nothing);
    }

    #[test]
    fn any_record_type() {
        // Line 512-513 — ANY RECORD
        let ty = parse_type("ANY RECORD");
        match ty {
            ValueType::Record {
                record_keyword,
                fields,
            } => {
                assert!(record_keyword);
                assert!(fields.is_empty());
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn any_value_union_type() {
        // Lines 517-524 — ANY VALUE <type | type>
        let ty = parse_type("ANY VALUE <INT32 | STRING>");
        match ty {
            ValueType::ClosedDynamicUnion(members) => {
                assert_eq!(members.len(), 2);
                assert!(matches!(members[0], ValueType::Int32 { .. }));
                assert!(matches!(members[1], ValueType::String { .. }));
            }
            other => panic!("expected ClosedDynamicUnion, got {other:?}"),
        }
    }

    #[test]
    fn any_angle_union_type() {
        // Lines 528-534 — ANY <type | type>
        let ty = parse_type("ANY <INT32 | STRING>");
        match ty {
            ValueType::ClosedDynamicUnion(members) => {
                assert_eq!(members.len(), 2);
                assert!(matches!(members[0], ValueType::Int32 { .. }));
                assert!(matches!(members[1], ValueType::String { .. }));
            }
            other => panic!("expected ClosedDynamicUnion, got {other:?}"),
        }
    }

    #[test]
    fn any_value_union_three_members() {
        let ty = parse_type("ANY VALUE <INT32 | STRING | BOOL>");
        match ty {
            ValueType::ClosedDynamicUnion(members) => {
                assert_eq!(members.len(), 3);
            }
            other => panic!("expected ClosedDynamicUnion, got {other:?}"),
        }
    }

    #[test]
    fn binding_table_type() {
        // Lines 604-612 — BINDING TABLE
        let ty = parse_type("BINDING TABLE");
        match ty {
            ValueType::BindingTableRef { .. } => {}
            other => panic!("expected BindingTableRef, got {other:?}"),
        }
    }

    #[test]
    fn property_graph_type() {
        // Lines 597-600
        let ty = parse_type("PROPERTY GRAPH");
        match ty {
            ValueType::GraphRef { .. } => {}
            other => panic!("expected GraphRef, got {other:?}"),
        }
    }

    #[test]
    fn any_node_type() {
        // Lines 499-502
        let ty = parse_type("ANY NODE");
        match ty {
            ValueType::NodeRef { .. } => {}
            other => panic!("expected NodeRef, got {other:?}"),
        }
    }

    #[test]
    fn any_vertex_type() {
        let ty = parse_type("ANY VERTEX");
        match ty {
            ValueType::NodeRef { .. } => {}
            other => panic!("expected NodeRef, got {other:?}"),
        }
    }

    #[test]
    fn any_edge_type() {
        // Lines 505-508
        let ty = parse_type("ANY EDGE");
        match ty {
            ValueType::EdgeRef { .. } => {}
            other => panic!("expected EdgeRef, got {other:?}"),
        }
    }

    #[test]
    fn any_relationship_type() {
        let ty = parse_type("ANY RELATIONSHIP");
        match ty {
            ValueType::EdgeRef { .. } => {}
            other => panic!("expected EdgeRef, got {other:?}"),
        }
    }

    #[test]
    fn any_property_value_type() {
        // Lines 486-487
        let ty = parse_type("ANY PROPERTY VALUE");
        assert_eq!(ty, ValueType::AnyPropertyValue);
    }

    #[test]
    fn bare_record_type() {
        // Lines 464-465 — bare RECORD (no braces)
        let ty = parse_type("RECORD");
        match ty {
            ValueType::Record {
                record_keyword,
                fields,
            } => {
                assert!(record_keyword);
                assert!(fields.is_empty());
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn int_type() {
        // Line 189 — INT parsed as Int32
        let ty = parse_type("INT");
        match ty {
            ValueType::Int32 { .. } => {}
            other => panic!("expected Int32, got {other:?}"),
        }
    }

    #[test]
    fn signed_integer_error_path() {
        // Line 700 — error after SIGNED (not followed by integer type)
        // Parsing SIGNED alone should fail
        let result = gleaph_gql::parser::parse("SESSION SET VALUE $x :: SIGNED = 42");
        assert!(result.is_err());
    }
}
