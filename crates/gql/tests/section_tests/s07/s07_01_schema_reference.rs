//! §7.1 — sessionSetSchemaClause: `SCHEMA schemaReference`.
//!
//! GQL §17.1 `schemaReference`:
//!   : absoluteCatalogSchemaReference
//!   | relativeCatalogSchemaReference
//!   | referenceParameterSpecification
//!   ;
//!
//! `SESSION SET SCHEMA` shares the reference parser used by the `AT` clause, so
//! each alternative must reach the same [`SchemaReference`] variant it reaches there.
//!
//! Construct: `s07.1-session-schema-reference`

use crate::section_tests::p;
use gleaph_gql::ast::*;

/// Parses `input` and returns the `SchemaReference` carried by `SESSION SET SCHEMA`.
fn schema_reference(input: &str) -> SchemaReference {
    let prog = p(input);
    assert_eq!(prog.session_activity.len(), 1);
    match &prog.session_activity[0] {
        SessionCommand::Set(SessionSetCommand::Schema(reference)) => reference.clone(),
        other => panic!("expected SessionSetCommand::Schema, got {other:?}"),
    }
}

mod absolute_catalog_schema_reference {
    use super::*;

    /// s07.1-session-schema-reference — `/a/b` → Absolute(["a", "b"])
    #[test]
    fn absolute_path() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA /a/b"),
            SchemaReference::Absolute(vec!["a".into(), "b".into()])
        );
    }

    /// s07.1-session-schema-reference — `/` (root) → Absolute([])
    #[test]
    fn absolute_root() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA /"),
            SchemaReference::Absolute(Vec::new())
        );
    }
}

mod predefined_schema_reference {
    use super::*;

    /// s07.1-session-schema-reference — `HOME_SCHEMA` → Current("HOME_SCHEMA")
    #[test]
    fn home_schema() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA HOME_SCHEMA"),
            SchemaReference::Current("HOME_SCHEMA".into())
        );
    }

    /// s07.1-session-schema-reference — `CURRENT_SCHEMA` → Current("CURRENT_SCHEMA")
    #[test]
    fn current_schema() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA CURRENT_SCHEMA"),
            SchemaReference::Current("CURRENT_SCHEMA".into())
        );
    }

    /// s07.1-session-schema-reference — `.` (current directory) → Current(".")
    #[test]
    fn current_directory() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA ."),
            SchemaReference::Current(".".into())
        );
    }
}

mod relative_catalog_schema_reference {
    use super::*;

    /// s07.1-session-schema-reference — `../a/b` → Relative(["..", "a", "b"])
    #[test]
    fn relative_path() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA ../a/b"),
            SchemaReference::Relative(vec!["..".into(), "a".into(), "b".into()])
        );
    }

    /// s07.1-session-schema-reference — `..` alone → Relative([".."])
    #[test]
    fn relative_parent_only() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA .."),
            SchemaReference::Relative(vec!["..".into()])
        );
    }
}

mod reference_parameter_specification {
    use super::*;

    /// s07.1-session-schema-reference — `$$p` → Parameter("p")
    #[test]
    fn parameter() {
        assert_eq!(
            schema_reference("SESSION SET SCHEMA $$p"),
            SchemaReference::Parameter("p".into())
        );
    }
}
