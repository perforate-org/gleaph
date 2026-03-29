//! §7.2 — Session reset command.
//!
//! GQL rules: sessionResetCommand, sessionResetArguments.

use crate::section_tests::p;
use gleaph_gql::ast::*;

// ── sessionResetCommand ──────────────────────────────────────────────────
//   : SESSION RESET sessionResetArguments?
//   ;
mod session_reset_command {
    use super::*;

    /// SESSION RESET  (no arguments — reset all)
    #[test]
    fn no_arguments() {
        let prog = p("SESSION RESET");
        assert_eq!(prog.session_activity.len(), 1);
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::All)
        ));
    }

    /// SESSION RESET with arguments
    #[test]
    fn with_arguments() {
        let prog = p("SESSION RESET SCHEMA");
        assert_eq!(prog.session_activity.len(), 1);
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::Schema)
        ));
    }
}

// ── sessionResetArguments ────────────────────────────────────────────────
//   : ALL? (PARAMETERS | CHARACTERISTICS)
//   | SCHEMA
//   | PROPERTY? GRAPH
//   | TIME ZONE
//   | PARAMETER? sessionParameterSpecification
//   ;
mod session_reset_arguments {
    use super::*;

    /// PARAMETERS
    #[test]
    fn parameters() {
        let prog = p("SESSION RESET PARAMETERS");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::AllParameters { .. })
        ));
    }

    /// ALL PARAMETERS
    #[test]
    fn all_parameters() {
        let prog = p("SESSION RESET ALL PARAMETERS");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::AllParameters { .. })
        ));
    }

    /// CHARACTERISTICS
    #[test]
    fn characteristics() {
        let prog = p("SESSION RESET CHARACTERISTICS");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::AllCharacteristics { .. })
        ));
    }

    /// ALL CHARACTERISTICS
    #[test]
    fn all_characteristics() {
        let prog = p("SESSION RESET ALL CHARACTERISTICS");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::AllCharacteristics { .. })
        ));
    }

    /// SCHEMA
    #[test]
    fn schema() {
        let prog = p("SESSION RESET SCHEMA");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::Schema)
        ));
    }

    /// GRAPH
    #[test]
    fn graph() {
        let prog = p("SESSION RESET GRAPH");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::Graph { .. })
        ));
    }

    /// PROPERTY GRAPH
    #[test]
    fn property_graph() {
        let prog = p("SESSION RESET PROPERTY GRAPH");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::Graph { .. })
        ));
    }

    /// TIME ZONE
    #[test]
    fn time_zone() {
        let prog = p("SESSION RESET TIME ZONE");
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(SessionResetCommand::TimeZone)
        ));
    }

    /// PARAMETER $name
    #[test]
    fn parameter_named() {
        let prog = p("SESSION RESET PARAMETER $x");
        match &prog.session_activity[0] {
            SessionCommand::Reset(SessionResetCommand::Parameter { name, .. }) => {
                assert_eq!(name, "x");
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }

    /// $name  (bare sessionParameterSpecification, no PARAMETER keyword)
    #[test]
    fn bare_param() {
        let prog = p("SESSION RESET $x");
        match &prog.session_activity[0] {
            SessionCommand::Reset(SessionResetCommand::Parameter { name, .. }) => {
                assert_eq!(name, "x");
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }
}
