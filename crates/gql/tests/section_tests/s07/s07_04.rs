//! §7.4 — Session parameter specification.
//!
//! GQL rule: sessionParameterSpecification.

use crate::section_tests::p;
use gleaph_gql::ast::*;

// ── sessionParameterSpecification ────────────────────────────────────────
//   : GENERAL_PARAMETER_REFERENCE
//   ;
mod session_parameter_specification {
    use super::*;

    /// GENERAL_PARAMETER_REFERENCE ($name) in SET context
    #[test]
    fn param_ref_in_set() {
        let prog = p("SESSION SET VALUE $myParam = 99");
        match &prog.session_activity[0] {
            SessionCommand::Set(SessionSetCommand::Parameter { name, .. }) => {
                assert_eq!(name, "myParam");
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }

    /// GENERAL_PARAMETER_REFERENCE ($name) in RESET context
    #[test]
    fn param_ref_in_reset() {
        let prog = p("SESSION RESET $myParam");
        match &prog.session_activity[0] {
            SessionCommand::Reset(SessionResetCommand::Parameter { name, .. }) => {
                assert_eq!(name, "myParam");
            }
            other => panic!("expected Parameter, got {other:?}"),
        }
    }
}
