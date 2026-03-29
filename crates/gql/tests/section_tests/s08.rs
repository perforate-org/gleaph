//! §8 — Transaction commands.
//!
//! GQL rules: startTransactionCommand, transactionCharacteristics,
//! transactionMode, transactionAccessMode, rollbackCommand, commitCommand.

use super::*;
use gleaph_gql::ast::*;

// ── startTransactionCommand ─────────────────────────────────────────────
//   : START TRANSACTION transactionCharacteristics?
//   ;
mod start_transaction_command {
    use super::*;

    /// START TRANSACTION  (no characteristics)
    #[test]
    fn no_characteristics() {
        let prog = p("START TRANSACTION MATCH (n) RETURN n");
        let t = ta(&prog);
        let start = t.start.as_ref().unwrap();
        assert!(start.access_modes.is_empty());
    }

    /// START TRANSACTION transactionCharacteristics
    #[test]
    fn with_characteristics() {
        let prog = p("START TRANSACTION READ ONLY MATCH (n) RETURN n");
        let t = ta(&prog);
        let start = t.start.as_ref().unwrap();
        assert_eq!(start.access_modes.len(), 1);
        assert_eq!(start.access_modes[0], TransactionAccessMode::ReadOnly);
    }
}

// ── transactionCharacteristics ──────────────────────────────────────────
//   : transactionMode (COMMA transactionMode)*
//   ;
mod transaction_characteristics {
    use super::*;

    /// Single mode
    #[test]
    fn single_mode() {
        let prog = p("START TRANSACTION READ WRITE MATCH (n) RETURN n");
        let t = ta(&prog);
        let start = t.start.as_ref().unwrap();
        assert_eq!(start.access_modes.len(), 1);
        assert_eq!(start.access_modes[0], TransactionAccessMode::ReadWrite);
    }

    /// Multiple modes (comma-separated)
    #[test]
    fn multiple_modes() {
        let prog = p("START TRANSACTION READ ONLY, READ WRITE MATCH (n) RETURN n");
        let t = ta(&prog);
        let start = t.start.as_ref().unwrap();
        assert_eq!(start.access_modes.len(), 2);
        assert_eq!(start.access_modes[0], TransactionAccessMode::ReadOnly);
        assert_eq!(start.access_modes[1], TransactionAccessMode::ReadWrite);
    }
}

// ── transactionAccessMode ───────────────────────────────────────────────
//   : READ ONLY
//   | READ WRITE
//   ;
mod transaction_access_mode {
    use super::*;

    /// READ ONLY
    #[test]
    fn read_only() {
        let prog = p("START TRANSACTION READ ONLY MATCH (n) RETURN n");
        let t = ta(&prog);
        let start = t.start.as_ref().unwrap();
        assert_eq!(start.access_modes[0], TransactionAccessMode::ReadOnly);
    }

    /// READ WRITE
    #[test]
    fn read_write() {
        let prog = p("START TRANSACTION READ WRITE MATCH (n) RETURN n");
        let t = ta(&prog);
        let start = t.start.as_ref().unwrap();
        assert_eq!(start.access_modes[0], TransactionAccessMode::ReadWrite);
    }
}

// ── rollbackCommand ─────────────────────────────────────────────────────
//   : ROLLBACK
//   ;
mod rollback_command {
    use super::*;

    /// ROLLBACK
    #[test]
    fn rollback() {
        let prog = p("ROLLBACK");
        let t = ta(&prog);
        assert_eq!(t.end, Some(TransactionEnd::Rollback));
    }
}

// ── commitCommand ───────────────────────────────────────────────────────
//   : COMMIT
//   ;
mod commit_command {
    use super::*;

    /// COMMIT
    #[test]
    fn commit() {
        let prog = p("COMMIT");
        let t = ta(&prog);
        assert_eq!(t.end, Some(TransactionEnd::Commit));
    }
}
