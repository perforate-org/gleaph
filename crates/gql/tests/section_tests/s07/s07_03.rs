//! §7.3 — Session close command.
//!
//! GQL rule: sessionCloseCommand.

use crate::section_tests::p;
use gleaph_gql::ast::*;

// ── sessionCloseCommand ──────────────────────────────────────────────────
//   : SESSION CLOSE
//   ;
mod session_close_command {
    use super::*;

    /// SESSION CLOSE
    #[test]
    fn session_close() {
        let prog = p("SESSION CLOSE");
        assert!(
            prog.session_activity
                .iter()
                .any(|c| matches!(c, SessionCommand::Close))
        );
        assert!(prog.transaction_activity.is_none());
    }

    /// SESSION CLOSE after a query
    #[test]
    fn session_close_trailing() {
        let prog = p("MATCH (n) RETURN n SESSION CLOSE");
        assert!(
            prog.session_activity
                .iter()
                .any(|c| matches!(c, SessionCommand::Close))
        );
        assert!(prog.transaction_activity.is_some());
    }
}
