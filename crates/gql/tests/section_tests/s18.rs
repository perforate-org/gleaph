//! §18 — Graph type specification & value types.
//!
//! GQL rules: nestedGraphTypeSpecification, graphTypeDefinitionBody,
//! nodeTypeDefinition, edgeTypeDefinition, propertyDefinition,
//! valueType, fieldType.

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

/// Extract a `ValueType` from `SESSION SET VALUE $x :: <type> = 0`.
fn extract_type(input: &str) -> ValueType {
    let prog = p(input);
    match &prog.session_activity[0] {
        SessionCommand::Set(SessionSetCommand::Parameter {
            type_annotation, ..
        }) => match type_annotation.as_ref().unwrap() {
            BindingTypeAnnotation::Value(vt) => vt.clone(),
            other => panic!("expected Value type annotation, got {other:?}"),
        },
        other => panic!("expected Parameter, got {other:?}"),
    }
}

// ── nestedGraphTypeSpecification ────────────────────────────────────────
//   : LEFT_BRACE graphTypeDefinitionBody RIGHT_BRACE
//   ;
mod nested_graph_type_specification {
    use super::*;

    /// CREATE GRAPH with inline node type
    #[test]
    fn inline_node_type() {
        let prog = p("CREATE GRAPH myGraph {(Person :Person {name STRING})}");
        let b = body(&prog);
        match &b.first {
            Statement::CreateGraph(cg) => {
                let gt = cg.graph_type.as_ref().expect("expected graph_type");
                match gt {
                    GraphTypeSpec::Inline(def) => {
                        assert!(!def.elements.is_empty());
                        let node = def
                            .elements
                            .iter()
                            .find(|e| matches!(e, GraphTypeElement::Node(_)));
                        assert!(node.is_some(), "expected at least one Node element");
                        if let Some(GraphTypeElement::Node(n)) = node {
                            assert_eq!(n.name.as_deref(), Some("Person"));
                            assert!(!n.properties.is_empty());
                        }
                    }
                    other => panic!("expected Inline graph type, got {other:?}"),
                }
            }
            other => panic!("expected CreateGraph, got {other:?}"),
        }
    }

    /// CREATE GRAPH with pattern syntax including edge
    #[test]
    fn pattern_with_edge() {
        let prog = p("CREATE GRAPH myGraph {(A :A), (B :B), (A)-[R :R]->(B)}");
        let b = body(&prog);
        match &b.first {
            Statement::CreateGraph(cg) => {
                let gt = cg.graph_type.as_ref().expect("expected graph_type");
                match gt {
                    GraphTypeSpec::Inline(def) => {
                        let edge = def
                            .elements
                            .iter()
                            .find(|e| matches!(e, GraphTypeElement::Edge(_)));
                        assert!(edge.is_some(), "expected at least one Edge element");
                    }
                    other => panic!("expected Inline graph type, got {other:?}"),
                }
            }
            other => panic!("expected CreateGraph, got {other:?}"),
        }
    }
}

// ── value_types_boolean ─────────────────────────────────────────────────
mod value_types_boolean {
    use super::*;

    /// BOOL keyword
    #[test]
    fn bool_keyword() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: BOOL = true"),
            ValueType::Bool { .. }
        ));
    }

    /// BOOLEAN keyword (alias)
    #[test]
    fn boolean_keyword() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: BOOLEAN = true"),
            ValueType::Bool { .. }
        ));
    }
}

// ── value_types_string ──────────────────────────────────────────────────
mod value_types_string {
    use super::*;

    /// STRING — no length constraints
    #[test]
    fn string_bare() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: STRING = 'hello'"),
            ValueType::String {
                min_length: None,
                max_length: None
            }
        );
    }

    /// STRING(100) — max_length
    #[test]
    fn string_with_max_length() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: STRING(100) = 'hello'"),
            ValueType::String {
                min_length: None,
                max_length: Some(100)
            }
        );
    }

    /// CHAR(10) — fixed-length character type
    #[test]
    fn char_with_length() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: CHAR(10) = 'hello'"),
            ValueType::Char {
                length: Some(10),
                ..
            }
        ));
    }

    /// VARCHAR(255) — variable-length character type
    #[test]
    fn varchar_with_max_length() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: VARCHAR(255) = 'hello'"),
            ValueType::Varchar {
                max_length: Some(255),
                ..
            }
        ));
    }
}

// ── value_types_bytes ───────────────────────────────────────────────────
mod value_types_bytes {
    use super::*;

    /// BYTES — no length constraint
    #[test]
    fn bytes_bare() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: BYTES = X'00'"),
            ValueType::Bytes { max_length: None }
        );
    }

    /// BINARY(16) — fixed-length binary
    #[test]
    fn binary_with_length() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: BINARY(16) = X'00'"),
            ValueType::Binary { length: Some(16) }
        );
    }

    /// VARBINARY(256) — variable-length binary
    #[test]
    fn varbinary_with_max_length() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: VARBINARY(256) = X'00'"),
            ValueType::Varbinary {
                max_length: Some(256),
                ..
            }
        ));
    }
}

// ── value_types_signed_integers ─────────────────────────────────────────
mod value_types_signed_integers {
    use super::*;

    #[test]
    fn int8() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT8 = 0"),
            ValueType::Int8 { .. }
        ));
    }

    #[test]
    fn int16() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT16 = 0"),
            ValueType::Int16 { .. }
        ));
    }

    #[test]
    fn int32() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT32 = 0"),
            ValueType::Int32 { .. }
        ));
    }

    #[test]
    fn int64() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT64 = 0"),
            ValueType::Int64 { .. }
        ));
    }

    #[test]
    fn int128() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT128 = 0"),
            ValueType::Int128 { .. }
        ));
    }

    #[test]
    fn int256() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT256 = 0"),
            ValueType::Int256 { .. }
        ));
    }

    /// INT(10) — precision-parameterized integer
    #[test]
    fn int_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT(10) = 0"),
            ValueType::IntPrecision { precision: 10, .. }
        ));
    }
}

// ── value_types_unsigned_integers ───────────────────────────────────────
mod value_types_unsigned_integers {
    use super::*;

    #[test]
    fn uint8() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT8 = 0"),
            ValueType::Uint8 { .. }
        ));
    }

    #[test]
    fn uint16() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT16 = 0"),
            ValueType::Uint16 { .. }
        ));
    }

    #[test]
    fn uint32() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT32 = 0"),
            ValueType::Uint32 { .. }
        ));
    }

    #[test]
    fn uint64() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT64 = 0"),
            ValueType::Uint64 { .. }
        ));
    }

    #[test]
    fn uint128() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT128 = 0"),
            ValueType::Uint128 { .. }
        ));
    }

    #[test]
    fn uint256() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT256 = 0"),
            ValueType::Uint256 { .. }
        ));
    }

    /// UINT(10) — precision-parameterized unsigned integer
    #[test]
    fn uint_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT(10) = 0"),
            ValueType::UintPrecision { precision: 10, .. }
        ));
    }
}

// ── value_types_decimal ─────────────────────────────────────────────────
mod value_types_decimal {
    use super::*;

    /// DECIMAL — no precision or scale
    #[test]
    fn decimal_bare() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: DECIMAL = 0"),
            ValueType::Decimal {
                precision: None,
                scale: None,
                ..
            }
        ));
    }

    /// DECIMAL(10) — precision only
    #[test]
    fn decimal_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: DECIMAL(10) = 0"),
            ValueType::Decimal {
                precision: Some(10),
                scale: None,
                ..
            }
        ));
    }

    /// DECIMAL(10, 2) — precision and scale
    #[test]
    fn decimal_with_precision_and_scale() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: DECIMAL(10, 2) = 0"),
            ValueType::Decimal {
                precision: Some(10),
                scale: Some(2),
                ..
            }
        ));
    }
}

// ── value_types_float ───────────────────────────────────────────────────
mod value_types_float {
    use super::*;

    #[test]
    fn float16() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: FLOAT16 = 0"),
            ValueType::Float16 { .. }
        ));
    }

    #[test]
    fn float32() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: FLOAT32 = 0"),
            ValueType::Float32 { .. }
        ));
    }

    #[test]
    fn float64() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: FLOAT64 = 0"),
            ValueType::Float64 { .. }
        ));
    }

    #[test]
    fn float128() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: FLOAT128 = 0"),
            ValueType::Float128
        );
    }

    #[test]
    fn float256() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: FLOAT256 = 0"),
            ValueType::Float256
        );
    }

    /// FLOAT(24) — precision-parameterized float
    #[test]
    fn float_with_precision() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: FLOAT(24) = 0"),
            ValueType::FloatPrecision {
                precision: 24,
                scale: None
            }
        );
    }

    /// REAL — alias for FLOAT32
    #[test]
    fn real() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: REAL = 0"),
            ValueType::Float32 { .. }
        ));
    }

    /// DOUBLE — alias for FLOAT64
    #[test]
    fn double() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: DOUBLE = 0"),
            ValueType::Float64 { .. }
        ));
    }

    /// DOUBLE PRECISION — alias for FLOAT64
    #[test]
    fn double_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: DOUBLE PRECISION = 0"),
            ValueType::Float64 { .. }
        ));
    }
}

// ── value_types_temporal ────────────────────────────────────────────────
mod value_types_temporal {
    use super::*;

    #[test]
    fn date() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: DATE = DATE '2024-01-01'"),
            ValueType::Date
        );
    }

    #[test]
    fn local_time() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LOCAL TIME = TIME '12:00:00'"),
            ValueType::LocalTime { .. }
        ));
    }

    #[test]
    fn local_datetime() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LOCAL DATETIME = DATETIME '2024-01-01T00:00:00'"),
            ValueType::LocalDateTime { .. }
        ));
    }

    #[test]
    fn zoned_time() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ZONED TIME = TIME '12:00:00+00:00'"),
            ValueType::ZonedTime { .. }
        ));
    }

    #[test]
    fn zoned_datetime() {
        assert!(matches!(
            extract_type(
                "SESSION SET VALUE $x :: ZONED DATETIME = DATETIME '2024-01-01T00:00:00+00:00'"
            ),
            ValueType::ZonedDateTime { .. }
        ));
    }

    #[test]
    fn timestamp_bare() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: TIMESTAMP = 0"),
            ValueType::Timestamp
        );
    }

    #[test]
    fn timestamp_with_timezone() {
        // Parser resolves TIMESTAMP WITH TIME ZONE → ZonedDateTime
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: TIMESTAMP WITH TIME ZONE = 0"),
            ValueType::ZonedDateTime { .. }
        ));
    }

    #[test]
    fn duration_year_to_month() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: DURATION(YEAR TO MONTH) = 0"),
            ValueType::DurationYearToMonth
        );
    }

    #[test]
    fn duration_day_to_second() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: DURATION(DAY TO SECOND) = 0"),
            ValueType::DurationDayToSecond
        );
    }
}

// ── value_types_reference ───────────────────────────────────────────────
mod value_types_reference {
    use super::*;

    #[test]
    fn node_ref() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: NODE = 0"),
            ValueType::NodeRef { label: None, .. }
        ));
    }

    #[test]
    fn edge_ref() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: EDGE = 0"),
            ValueType::EdgeRef { label: None, .. }
        ));
    }

    #[test]
    fn path() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: PATH = 0"),
            ValueType::Path
        );
    }
}

// ── value_types_list ────────────────────────────────────────────────────
mod value_types_list {
    use super::*;

    /// LIST<INT32> — list with element type
    #[test]
    fn list_generic() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LIST<INT32> = 0"),
            ValueType::List {
                max_length: None,
                ..
            }
        ));
    }

    /// LIST<INT32>[10] — list with max length
    #[test]
    fn list_with_max_length() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LIST<INT32>[10] = 0"),
            ValueType::List {
                max_length: Some(10),
                ..
            }
        ));
    }

    /// INT32 LIST — postfix list syntax
    #[test]
    fn list_postfix() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT32 LIST = 0"),
            ValueType::List {
                max_length: None,
                ..
            }
        ));
    }
}

// ── value_types_record ──────────────────────────────────────────────────
mod value_types_record {
    use super::*;

    /// RECORD {name STRING, age INT32} — explicit RECORD keyword
    #[test]
    fn record_explicit() {
        let vt = extract_type("SESSION SET VALUE $x :: RECORD {name STRING, age INT32} = 0");
        match vt {
            ValueType::Record { ref fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "name");
                assert_eq!(
                    fields[0].value_type,
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
                assert_eq!(fields[1].name, "age");
                assert!(matches!(fields[1].value_type, ValueType::Int32 { .. }));
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    /// {name STRING} — bare braces record
    #[test]
    fn record_bare_braces() {
        let vt = extract_type("SESSION SET VALUE $x :: {name STRING} = 0");
        match vt {
            ValueType::Record { ref fields, .. } => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].name, "name");
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }
}

// ── value_types_dynamic_union ───────────────────────────────────────────
mod value_types_dynamic_union {
    use super::*;

    /// ANY — open dynamic union
    #[test]
    fn any() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: ANY = 0"),
            ValueType::Any
        );
    }

    /// ANY VALUE — any value type
    #[test]
    fn any_value() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: ANY VALUE = 0"),
            ValueType::AnyValue
        );
    }

    /// ANY PROPERTY VALUE — any property value type
    #[test]
    fn any_property_value() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: ANY PROPERTY VALUE = 0"),
            ValueType::AnyPropertyValue
        );
    }

    /// INT32 | STRING — closed dynamic union
    #[test]
    fn closed_dynamic_union() {
        let vt = extract_type("SESSION SET VALUE $x :: INT32 | STRING = 0");
        match vt {
            ValueType::ClosedDynamicUnion(ref types) => {
                assert_eq!(types.len(), 2);
                assert!(matches!(types[0], ValueType::Int32 { .. }));
                assert_eq!(
                    types[1],
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
            }
            other => panic!("expected ClosedDynamicUnion, got {other:?}"),
        }
    }
}

// ── value_types_immaterial ──────────────────────────────────────────────
mod value_types_immaterial {
    use super::*;

    /// NULL type
    #[test]
    fn null_type() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: NULL = NULL"),
            ValueType::Null
        );
    }

    /// NOTHING type
    #[test]
    fn nothing_type() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: NOTHING = NULL"),
            ValueType::Nothing
        );
    }

    /// INT32 NOT NULL — not-null wrapper
    #[test]
    fn not_null() {
        match extract_type("SESSION SET VALUE $x :: INT32 NOT NULL = 0") {
            ValueType::NotNull(inner) => assert!(matches!(*inner, ValueType::Int32 { .. })),
            other => panic!("expected NotNull, got {other:?}"),
        }
    }
}

// ── field_type ──────────────────────────────────────────────────────────
//   : fieldName TYPED? valueType
//   ;
mod field_type {
    use super::*;

    /// RECORD {name TYPED STRING, age :: INT32} — verify fields parse correctly
    #[test]
    fn typed_and_double_colon() {
        let vt =
            extract_type("SESSION SET VALUE $x :: RECORD {name TYPED STRING, age :: INT32} = 0");
        match vt {
            ValueType::Record { ref fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "name");
                assert_eq!(
                    fields[0].value_type,
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
                assert_eq!(fields[1].name, "age");
                assert!(matches!(fields[1].value_type, ValueType::Int32 { .. }));
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }
}

// ── additional coverage: STRING/BYTES parameterised forms ───────────────
mod value_types_string_parameterised {
    use super::*;

    /// STRING(min, max) — both bounds
    #[test]
    fn string_with_min_and_max() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: STRING(5, 100) = 'hello'"),
            ValueType::String {
                min_length: Some(5),
                max_length: Some(100)
            }
        );
    }
}

mod value_types_bytes_parameterised {
    use super::*;

    /// BYTES(max) — single bound
    #[test]
    fn bytes_with_max_length() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: BYTES(256) = X'00'"),
            ValueType::Bytes {
                max_length: Some(256)
            }
        );
    }

    /// BYTES(min, max) — two bounds (parser stores only max)
    #[test]
    fn bytes_with_min_and_max() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: BYTES(10, 256) = X'00'"),
            ValueType::Bytes {
                max_length: Some(256)
            }
        );
    }
}

// ── additional coverage: BINARY / VARBINARY bare forms ──────────────────
mod value_types_binary_bare {
    use super::*;

    /// BINARY — no length
    #[test]
    fn binary_bare() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: BINARY = X'00'"),
            ValueType::Binary { length: None }
        );
    }

    /// VARBINARY — no length
    #[test]
    fn varbinary_bare() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: VARBINARY = X'00'"),
            ValueType::Varbinary {
                max_length: None,
                ..
            }
        ));
    }

    /// BINARY VARYING(128) — verbose varbinary
    #[test]
    fn binary_varying() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: BINARY VARYING(128) = X'00'"),
            ValueType::Varbinary {
                max_length: Some(128),
                ..
            }
        ));
    }
}

// ── additional coverage: DECIMAL parameterised forms ────────────────────
mod value_types_decimal_extra {
    use super::*;

    /// DEC — alias for DECIMAL
    #[test]
    fn dec_alias() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: DEC = 0"),
            ValueType::Decimal {
                precision: None,
                scale: None,
                ..
            }
        ));
    }

    /// NUMERIC(10, 2) — alias with precision and scale
    #[test]
    fn numeric_with_precision_and_scale() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: NUMERIC(10, 2) = 0"),
            ValueType::Decimal {
                precision: Some(10),
                scale: Some(2),
                ..
            }
        ));
    }
}

// ── additional coverage: PATH type ──────────────────────────────────────
mod value_types_path {
    use super::*;

    #[test]
    fn path_type() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: PATH = 0"),
            ValueType::Path
        );
    }
}

// ── additional coverage: LIST / ARRAY prefix and synonym ────────────────
mod value_types_list_extra {
    use super::*;

    /// ARRAY<INT32> — ARRAY synonym for LIST
    #[test]
    fn array_generic() {
        match extract_type("SESSION SET VALUE $x :: ARRAY<INT32> = 0") {
            ValueType::List {
                ref element_type,
                max_length,
                ..
            } => {
                assert!(matches!(**element_type, ValueType::Int32 { .. }));
                assert_eq!(max_length, None);
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    /// INT32 ARRAY — postfix ARRAY syntax
    #[test]
    fn array_postfix() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INT32 ARRAY = 0"),
            ValueType::List {
                max_length: None,
                ..
            }
        ));
    }

    /// bare LIST — no angle brackets
    #[test]
    fn list_bare() {
        match extract_type("SESSION SET VALUE $x :: LIST = 0") {
            ValueType::List {
                ref element_type,
                max_length,
                ..
            } => {
                assert_eq!(**element_type, ValueType::AnyValue);
                assert_eq!(max_length, None);
            }
            other => panic!("expected List, got {other:?}"),
        }
    }
}

// ── additional coverage: verbose integer keywords ───────────────────────
mod value_types_verbose_integers {
    use super::*;

    /// INTEGER8 (verbose form of INT8)
    #[test]
    fn integer8() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER8 = 0"),
            ValueType::Int8 { .. }
        ));
    }

    /// INTEGER16
    #[test]
    fn integer16() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER16 = 0"),
            ValueType::Int16 { .. }
        ));
    }

    /// INTEGER32
    #[test]
    fn integer32() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER32 = 0"),
            ValueType::Int32 { .. }
        ));
    }

    /// INTEGER64
    #[test]
    fn integer64() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER64 = 0"),
            ValueType::Int64 { .. }
        ));
    }

    /// INTEGER128
    #[test]
    fn integer128() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER128 = 0"),
            ValueType::Int128 { .. }
        ));
    }

    /// INTEGER256
    #[test]
    fn integer256() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER256 = 0"),
            ValueType::Int256 { .. }
        ));
    }

    /// SIGNED INTEGER8
    #[test]
    fn signed_integer8() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER8 = 0"),
            ValueType::Int8 { .. }
        ));
    }

    /// SIGNED INTEGER16
    #[test]
    fn signed_integer16() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER16 = 0"),
            ValueType::Int16 { .. }
        ));
    }

    /// SIGNED INTEGER32
    #[test]
    fn signed_integer32() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER32 = 0"),
            ValueType::Int32 { .. }
        ));
    }

    /// SIGNED INTEGER64
    #[test]
    fn signed_integer64() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER64 = 0"),
            ValueType::Int64 { .. }
        ));
    }

    /// SIGNED INTEGER128
    #[test]
    fn signed_integer128() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER128 = 0"),
            ValueType::Int128 { .. }
        ));
    }

    /// SIGNED INTEGER256
    #[test]
    fn signed_integer256() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER256 = 0"),
            ValueType::Int256 { .. }
        ));
    }

    /// UNSIGNED INTEGER8
    #[test]
    fn unsigned_integer8() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER8 = 0"),
            ValueType::Uint8 { .. }
        ));
    }

    /// UNSIGNED INTEGER16
    #[test]
    fn unsigned_integer16() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER16 = 0"),
            ValueType::Uint16 { .. }
        ));
    }

    /// UNSIGNED INTEGER32
    #[test]
    fn unsigned_integer32() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER32 = 0"),
            ValueType::Uint32 { .. }
        ));
    }

    /// UNSIGNED INTEGER64
    #[test]
    fn unsigned_integer64() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER64 = 0"),
            ValueType::Uint64 { .. }
        ));
    }

    /// UNSIGNED INTEGER128
    #[test]
    fn unsigned_integer128() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER128 = 0"),
            ValueType::Uint128 { .. }
        ));
    }

    /// UNSIGNED INTEGER256
    #[test]
    fn unsigned_integer256() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER256 = 0"),
            ValueType::Uint256 { .. }
        ));
    }
}

// ── additional coverage: SQL-compat integer aliases ─────────────────────
mod value_types_sql_compat_integers {
    use super::*;

    /// SMALL INTEGER — two-word form → Int16
    #[test]
    fn small_integer() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SMALL INTEGER = 0"),
            ValueType::Int16 { .. }
        ));
    }

    /// BIG INTEGER — two-word form → Int64
    #[test]
    fn big_integer() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: BIG INTEGER = 0"),
            ValueType::Int64 { .. }
        ));
    }

    /// SMALLINT — single word → Int16
    #[test]
    fn smallint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SMALLINT = 0"),
            ValueType::Int16 { .. }
        ));
    }

    /// BIGINT — single word → Int64
    #[test]
    fn bigint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: BIGINT = 0"),
            ValueType::Int64 { .. }
        ));
    }

    /// INTEGER — bare, no precision → Int32
    #[test]
    fn integer_bare() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER = 0"),
            ValueType::Int32 { .. }
        ));
    }

    /// INTEGER(10) — with precision
    #[test]
    fn integer_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: INTEGER(10) = 0"),
            ValueType::IntPrecision { precision: 10, .. }
        ));
    }

    /// TINYINT — sql-compat alias for INT8
    #[cfg(feature = "sql-compat")]
    #[test]
    fn tinyint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: TINYINT = 0"),
            ValueType::Int8 { .. }
        ));
    }

    /// SIGNED SMALL INTEGER
    #[test]
    fn signed_small_integer() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED SMALL INTEGER = 0"),
            ValueType::Int16 { .. }
        ));
    }

    /// SIGNED BIG INTEGER
    #[test]
    fn signed_big_integer() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED BIG INTEGER = 0"),
            ValueType::Int64 { .. }
        ));
    }

    /// SIGNED INTEGER — bare
    #[test]
    fn signed_integer_bare() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER = 0"),
            ValueType::Int32 { .. }
        ));
    }

    /// SIGNED INT
    #[test]
    fn signed_int() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INT = 0"),
            ValueType::Int32 { .. }
        ));
    }

    /// SIGNED SMALLINT
    #[test]
    fn signed_smallint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED SMALLINT = 0"),
            ValueType::Int16 { .. }
        ));
    }

    /// SIGNED BIGINT
    #[test]
    fn signed_bigint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED BIGINT = 0"),
            ValueType::Int64 { .. }
        ));
    }

    /// UNSIGNED SMALL INTEGER
    #[test]
    fn unsigned_small_integer() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED SMALL INTEGER = 0"),
            ValueType::Uint16 { .. }
        ));
    }

    /// UNSIGNED BIG INTEGER
    #[test]
    fn unsigned_big_integer() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED BIG INTEGER = 0"),
            ValueType::Uint64 { .. }
        ));
    }

    /// UNSIGNED INTEGER — bare
    #[test]
    fn unsigned_integer_bare() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER = 0"),
            ValueType::Uint32 { .. }
        ));
    }

    /// UNSIGNED INT
    #[test]
    fn unsigned_int() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INT = 0"),
            ValueType::Uint32 { .. }
        ));
    }

    /// UNSIGNED SMALLINT
    #[test]
    fn unsigned_smallint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED SMALLINT = 0"),
            ValueType::Uint16 { .. }
        ));
    }

    /// UNSIGNED BIGINT
    #[test]
    fn unsigned_bigint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED BIGINT = 0"),
            ValueType::Uint64 { .. }
        ));
    }

    /// UNSIGNED INTEGER(10) — with precision
    #[test]
    fn unsigned_integer_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INTEGER(10) = 0"),
            ValueType::UintPrecision { precision: 10, .. }
        ));
    }

    /// SIGNED INTEGER(10) — with precision
    #[test]
    fn signed_integer_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INTEGER(10) = 0"),
            ValueType::IntPrecision { precision: 10, .. }
        ));
    }

    /// SIGNED INT(10) — with precision
    #[test]
    fn signed_int_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: SIGNED INT(10) = 0"),
            ValueType::IntPrecision { precision: 10, .. }
        ));
    }

    /// UNSIGNED INT(10) — with precision
    #[test]
    fn unsigned_int_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UNSIGNED INT(10) = 0"),
            ValueType::UintPrecision { precision: 10, .. }
        ));
    }
}

// ── additional coverage: temporal two-word forms ────────────────────────
mod value_types_temporal_extra {
    use super::*;

    /// ZONED DATETIME — two-word form
    #[test]
    fn zoned_datetime_two_words() {
        assert!(matches!(
            extract_type(
                "SESSION SET VALUE $x :: ZONED DATETIME = DATETIME '2024-01-01T00:00:00+00:00'"
            ),
            ValueType::ZonedDateTime { .. }
        ));
    }

    /// LOCAL DATETIME — two-word form
    #[test]
    fn local_datetime_two_words() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LOCAL DATETIME = DATETIME '2024-01-01T00:00:00'"),
            ValueType::LocalDateTime { .. }
        ));
    }

    /// TIME WITH TIME ZONE
    #[test]
    fn time_with_time_zone() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: TIME WITH TIME ZONE = TIME '12:00:00+00:00'"),
            ValueType::ZonedTime { .. }
        ));
    }

    /// TIME WITHOUT TIME ZONE
    #[test]
    fn time_without_time_zone() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: TIME WITHOUT TIME ZONE = TIME '12:00:00'"),
            ValueType::LocalTime { .. }
        ));
    }

    /// TIMESTAMP WITHOUT TIME ZONE
    #[test]
    fn timestamp_without_time_zone() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: TIMESTAMP WITHOUT TIME ZONE = 0"),
            ValueType::LocalDateTime { .. }
        ));
    }

    /// LOCAL TIMESTAMP
    #[test]
    fn local_timestamp() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LOCAL TIMESTAMP = 0"),
            ValueType::LocalDateTime { .. }
        ));
    }
}

// ── additional coverage: ANY variants ───────────────────────────────────
mod value_types_any_variants {
    use super::*;

    /// ANY PROPERTY VALUE
    #[test]
    fn any_property_value() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: ANY PROPERTY VALUE = 0"),
            ValueType::AnyPropertyValue
        );
    }

    /// ANY VALUE
    #[test]
    fn any_value() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: ANY VALUE = 0"),
            ValueType::AnyValue
        );
    }

    /// ANY NODE
    #[test]
    fn any_node() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ANY NODE = 0"),
            ValueType::NodeRef { label: None, .. }
        ));
    }

    /// ANY EDGE
    #[test]
    fn any_edge() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ANY EDGE = 0"),
            ValueType::EdgeRef { label: None, .. }
        ));
    }

    /// ANY <INT32 | STRING> — closed dynamic union via ANY
    #[test]
    fn any_closed_dynamic_union() {
        let vt = extract_type("SESSION SET VALUE $x :: ANY <INT32 | STRING> = 0");
        match vt {
            ValueType::ClosedDynamicUnion(ref members) => {
                assert_eq!(members.len(), 2);
                assert!(matches!(members[0], ValueType::Int32 { .. }));
                assert_eq!(
                    members[1],
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
            }
            other => panic!("expected ClosedDynamicUnion, got {other:?}"),
        }
    }

    /// ANY VALUE <INT32 | STRING> — closed dynamic union via ANY VALUE
    #[test]
    fn any_value_closed_dynamic_union() {
        let vt = extract_type("SESSION SET VALUE $x :: ANY VALUE <INT32 | STRING> = 0");
        match vt {
            ValueType::ClosedDynamicUnion(ref members) => {
                assert_eq!(members.len(), 2);
                assert!(matches!(members[0], ValueType::Int32 { .. }));
                assert_eq!(
                    members[1],
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
            }
            other => panic!("expected ClosedDynamicUnion, got {other:?}"),
        }
    }
}

// ── additional coverage: record without RECORD keyword ──────────────────
mod value_types_record_extra {
    use super::*;

    /// {name STRING, age INT32} — bare braces, multiple fields
    #[test]
    fn bare_braces_multiple_fields() {
        let vt = extract_type("SESSION SET VALUE $x :: {name STRING, age INT32} = 0");
        match vt {
            ValueType::Record {
                record_keyword,
                ref fields,
                ..
            } => {
                assert!(!record_keyword);
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "name");
                assert_eq!(fields[1].name, "age");
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    /// RECORD {name TYPED STRING} — field with TYPED prefix
    #[test]
    fn field_with_typed_prefix() {
        let vt = extract_type("SESSION SET VALUE $x :: RECORD {name TYPED STRING} = 0");
        match vt {
            ValueType::Record { ref fields, .. } => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].name, "name");
                assert_eq!(fields[0].typed_prefix, TypedPrefix::Typed);
                assert_eq!(
                    fields[0].value_type,
                    ValueType::String {
                        min_length: None,
                        max_length: None
                    }
                );
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }
}

// ── additional coverage: misc uncovered keywords ────────────────────────
mod value_types_misc_extra {
    use super::*;

    /// CHARACTER VARYING(100) — verbose varchar
    #[test]
    fn character_varying() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: CHARACTER VARYING(100) = 'hello'"),
            ValueType::Varchar {
                max_length: Some(100),
                ..
            }
        ));
    }

    /// CHARACTER(10) — verbose char
    #[test]
    fn character_with_length() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: CHARACTER(10) = 'hello'"),
            ValueType::Char {
                length: Some(10),
                ..
            }
        ));
    }

    /// HALF — alias for FLOAT16
    #[test]
    fn half_float() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: HALF = 0"),
            ValueType::Float16 { .. }
        ));
    }

    /// USMALLINT — alias for UINT16
    #[test]
    fn usmallint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: USMALLINT = 0"),
            ValueType::Uint16 { .. }
        ));
    }

    /// UBIGINT — alias for UINT64
    #[test]
    fn ubigint() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UBIGINT = 0"),
            ValueType::Uint64 { .. }
        ));
    }

    /// UINT — bare, no precision → Uint32
    #[test]
    fn uint_bare() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: UINT = 0"),
            ValueType::Uint32 { .. }
        ));
    }

    /// DATETIME — bare datetime
    #[test]
    fn datetime_bare() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: DATETIME = 0"),
            ValueType::DateTime
        );
    }

    /// TIME — bare time
    #[test]
    fn time_bare() {
        assert_eq!(
            extract_type("SESSION SET VALUE $x :: TIME = 0"),
            ValueType::Time
        );
    }

    /// FLOAT — bare (no precision) → Float32
    #[test]
    fn float_bare() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: FLOAT = 0"),
            ValueType::Float32 { .. }
        ));
    }

    /// ZONED_DATETIME — single-token variant
    #[test]
    fn zoned_datetime_underscore() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ZONED_DATETIME = 0"),
            ValueType::ZonedDateTime { .. }
        ));
    }

    /// ZONED_TIME — single-token variant
    #[test]
    fn zoned_time_underscore() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ZONED_TIME = 0"),
            ValueType::ZonedTime { .. }
        ));
    }

    /// LOCAL_DATETIME — single-token variant
    #[test]
    fn local_datetime_underscore() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LOCAL_DATETIME = 0"),
            ValueType::LocalDateTime { .. }
        ));
    }

    /// LOCAL_TIME — single-token variant
    #[test]
    fn local_time_underscore() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LOCAL_TIME = 0"),
            ValueType::LocalTime { .. }
        ));
    }

    /// LOCAL_TIMESTAMP — single-token variant
    #[test]
    fn local_timestamp_underscore() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: LOCAL_TIMESTAMP = 0"),
            ValueType::LocalDateTime { .. }
        ));
    }

    /// VERTEX — alias for NODE
    #[test]
    fn vertex_ref() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: VERTEX = 0"),
            ValueType::NodeRef { label: None, .. }
        ));
    }

    /// RELATIONSHIP — alias for EDGE
    #[test]
    fn relationship_ref() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: RELATIONSHIP = 0"),
            ValueType::EdgeRef { label: None, .. }
        ));
    }

    /// GRAPH — bare graph ref
    #[test]
    fn graph_ref() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: GRAPH = 0"),
            ValueType::GraphRef { .. }
        ));
    }

    /// PROPERTY GRAPH — verbose graph ref
    #[test]
    fn property_graph_ref() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: PROPERTY GRAPH = 0"),
            ValueType::GraphRef { .. }
        ));
    }

    /// ANY PROPERTY GRAPH
    #[test]
    fn any_property_graph() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ANY PROPERTY GRAPH = 0"),
            ValueType::GraphRef { .. }
        ));
    }

    /// ANY GRAPH
    #[test]
    fn any_graph() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ANY GRAPH = 0"),
            ValueType::GraphRef { .. }
        ));
    }

    /// ANY VERTEX — alias for ANY NODE
    #[test]
    fn any_vertex() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ANY VERTEX = 0"),
            ValueType::NodeRef { label: None, .. }
        ));
    }

    /// ANY RELATIONSHIP — alias for ANY EDGE
    #[test]
    fn any_relationship() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: ANY RELATIONSHIP = 0"),
            ValueType::EdgeRef { label: None, .. }
        ));
    }

    /// NUMERIC(10) — alias with precision only
    #[test]
    fn numeric_with_precision() {
        assert!(matches!(
            extract_type("SESSION SET VALUE $x :: NUMERIC(10) = 0"),
            ValueType::Decimal {
                precision: Some(10),
                scale: None,
                ..
            }
        ));
    }
}

// ── Value types (parser/types.rs) ─────────────────────────────────────

/// Helper to extract a parsed value type from a session parameter annotation.
fn parse_type(type_str: &str) -> ValueType {
    let input = format!("SESSION SET VALUE $x :: {type_str} = 42");
    let prog = crate::section_tests::p(&input);
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
