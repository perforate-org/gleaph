//! §6 — GQL program structure.
//!
//! GQL rules: gqlProgram, programActivity, sessionActivity,
//! transactionActivity, endTransactionCommand, sessionCloseCommand,
//! procedureSpecification, procedureBody, bindingVariableDefinitionBlock,
//! bindingVariableDefinition, statementBlock, statement, nextStatement.

use super::*;
use gleaph_gql::ast::*;

// ── gqlProgram ───────────────────────────────────────────────────────────
//   : programActivity sessionCloseCommand? EOF
//   | sessionCloseCommand EOF
//   ;
mod gql_program {
    use super::*;

    /// programActivity EOF  (no sessionCloseCommand)
    #[test]
    fn program_activity_eof() {
        let prog = p("MATCH (n) RETURN n");
        assert!(prog.session_activity.is_empty());
        assert!(prog.transaction_activity.is_some());
    }

    /// programActivity sessionCloseCommand EOF
    #[test]
    fn program_activity_session_close_eof() {
        let prog = p("MATCH (n) RETURN n SESSION CLOSE");
        assert!(prog.transaction_activity.is_some());
        assert!(
            prog.session_activity
                .iter()
                .any(|c| matches!(c, SessionCommand::Close))
        );
    }

    /// sessionCloseCommand EOF
    #[test]
    fn session_close_eof() {
        let prog = p("SESSION CLOSE");
        assert!(
            prog.session_activity
                .iter()
                .any(|c| matches!(c, SessionCommand::Close))
        );
        assert!(prog.transaction_activity.is_none());
    }
}

// ── programActivity ──────────────────────────────────────────────────────
//   : sessionActivity
//   | transactionActivity
//   ;
mod program_activity {
    use super::*;

    /// sessionActivity
    #[test]
    fn session_activity() {
        let prog = p("SESSION SET SCHEMA /mydb");
        assert!(!prog.session_activity.is_empty());
        assert!(prog.transaction_activity.is_none());
    }

    /// transactionActivity
    #[test]
    fn transaction_activity() {
        let prog = p("MATCH (n) RETURN n");
        assert!(prog.session_activity.is_empty());
        assert!(prog.transaction_activity.is_some());
    }
}

// ── sessionActivity ──────────────────────────────────────────────────────
//   : sessionResetCommand+
//   | sessionSetCommand+ sessionResetCommand*
//   ;
mod session_activity {
    use super::*;

    /// sessionResetCommand+
    #[test]
    fn reset_commands() {
        let prog = p("SESSION RESET SCHEMA SESSION RESET GRAPH");
        assert_eq!(prog.session_activity.len(), 2);
        assert!(matches!(
            &prog.session_activity[0],
            SessionCommand::Reset(_)
        ));
        assert!(matches!(
            &prog.session_activity[1],
            SessionCommand::Reset(_)
        ));
    }

    /// sessionSetCommand+  (no resets)
    #[test]
    fn set_commands() {
        let prog = p("SESSION SET SCHEMA /a SESSION SET GRAPH g");
        assert_eq!(prog.session_activity.len(), 2);
        assert!(matches!(&prog.session_activity[0], SessionCommand::Set(_)));
        assert!(matches!(&prog.session_activity[1], SessionCommand::Set(_)));
    }

    /// sessionSetCommand+ sessionResetCommand*
    #[test]
    fn set_then_reset() {
        let prog = p("SESSION SET SCHEMA /a SESSION RESET GRAPH");
        assert_eq!(prog.session_activity.len(), 2);
        assert!(matches!(&prog.session_activity[0], SessionCommand::Set(_)));
        assert!(matches!(
            &prog.session_activity[1],
            SessionCommand::Reset(_)
        ));
    }
}

// ── transactionActivity ──────────────────────────────────────────────────
//   : startTransactionCommand (procedureSpecification endTransactionCommand?)?
//   | procedureSpecification endTransactionCommand?
//   | endTransactionCommand
//   ;
mod transaction_activity {
    use super::*;

    /// startTransactionCommand  (no body, no end)
    #[test]
    fn start_only() {
        let prog = p("START TRANSACTION");
        let t = ta(&prog);
        assert!(t.start.is_some());
        assert!(t.body.is_none());
        assert!(t.end.is_none());
    }

    /// startTransactionCommand procedureSpecification
    #[test]
    fn start_body() {
        let prog = p("START TRANSACTION MATCH (n) RETURN n");
        let t = ta(&prog);
        assert!(t.start.is_some());
        assert!(t.body.is_some());
        assert!(t.end.is_none());
    }

    /// startTransactionCommand procedureSpecification endTransactionCommand
    #[test]
    fn start_body_end() {
        let prog = p("START TRANSACTION MATCH (n) RETURN n COMMIT");
        let t = ta(&prog);
        assert!(t.start.is_some());
        assert!(t.body.is_some());
        assert_eq!(t.end, Some(TransactionEnd::Commit));
    }

    /// procedureSpecification  (no start, no end)
    #[test]
    fn body_only() {
        let prog = p("MATCH (n) RETURN n");
        let t = ta(&prog);
        assert!(t.start.is_none());
        assert!(t.body.is_some());
        assert!(t.end.is_none());
    }

    /// procedureSpecification endTransactionCommand
    #[test]
    fn body_end() {
        let prog = p("MATCH (n) RETURN n ROLLBACK");
        let t = ta(&prog);
        assert!(t.start.is_none());
        assert!(t.body.is_some());
        assert_eq!(t.end, Some(TransactionEnd::Rollback));
    }

    /// endTransactionCommand  (bare COMMIT)
    #[test]
    fn end_only_commit() {
        let prog = p("COMMIT");
        let t = ta(&prog);
        assert!(t.start.is_none());
        assert!(t.body.is_none());
        assert_eq!(t.end, Some(TransactionEnd::Commit));
    }

    /// endTransactionCommand  (bare ROLLBACK)
    #[test]
    fn end_only_rollback() {
        let prog = p("ROLLBACK");
        let t = ta(&prog);
        assert!(t.start.is_none());
        assert!(t.body.is_none());
        assert_eq!(t.end, Some(TransactionEnd::Rollback));
    }
}

// ── endTransactionCommand ────────────────────────────────────────────────
//   : rollbackCommand
//   | commitCommand
//   ;
mod end_transaction_command {
    use super::*;

    /// rollbackCommand
    #[test]
    fn rollback() {
        let prog = p("MATCH (n) RETURN n ROLLBACK");
        assert_eq!(ta(&prog).end, Some(TransactionEnd::Rollback));
    }

    /// commitCommand
    #[test]
    fn commit() {
        let prog = p("MATCH (n) RETURN n COMMIT");
        assert_eq!(ta(&prog).end, Some(TransactionEnd::Commit));
    }
}

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
    }
}

// ── procedureBody ────────────────────────────────────────────────────────
//   : atSchemaClause? bindingVariableDefinitionBlock? statementBlock
//   ;
mod procedure_body {
    use super::*;

    /// statementBlock only
    #[test]
    fn statement_block_only() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::Query(_)));
    }

    /// atSchemaClause statementBlock
    #[test]
    fn at_schema_statement_block() {
        let prog = p("AT /myschema MATCH (n) RETURN n");
        assert!(body(&prog).next.is_empty());
    }

    /// bindingVariableDefinitionBlock statementBlock
    #[test]
    fn bindings_statement_block() {
        let prog = p("VALUE x = 1 MATCH (n) WHERE n.id = x RETURN n");
        assert!(body(&prog).next.is_empty());
    }

    /// atSchemaClause bindingVariableDefinitionBlock statementBlock
    #[test]
    fn at_schema_bindings_statement_block() {
        let prog = p("AT /myschema VALUE x = 1 MATCH (n) RETURN n");
        assert!(body(&prog).next.is_empty());
    }
}

// ── bindingVariableDefinitionBlock ────────────────────────────────────────
//   : bindingVariableDefinition+
//   ;
mod binding_variable_definition_block {
    use super::*;

    /// Single binding
    #[test]
    fn single() {
        let prog = p("VALUE x = 1 MATCH (n) RETURN n");
        assert!(prog.transaction_activity.is_some());
    }

    /// Multiple bindings
    #[test]
    fn multiple() {
        let prog = p("VALUE x = 1 VALUE y = 2 MATCH (n) RETURN n");
        assert!(prog.transaction_activity.is_some());
    }
}

// ── bindingVariableDefinition ────────────────────────────────────────────
//   : graphVariableDefinition
//   | bindingTableVariableDefinition
//   | valueVariableDefinition
//   ;
mod binding_variable_definition {
    use super::*;

    /// graphVariableDefinition
    #[test]
    fn graph_variable() {
        let prog = p("GRAPH g = myGraph MATCH (n) RETURN n");
        assert!(prog.transaction_activity.is_some());
    }

    /// bindingTableVariableDefinition
    #[test]
    fn binding_table_variable() {
        let prog = p("BINDING TABLE t = $other MATCH (n) RETURN n");
        assert!(prog.transaction_activity.is_some());
    }

    /// valueVariableDefinition
    #[test]
    fn value_variable() {
        let prog = p("VALUE x = 42 MATCH (n) RETURN n");
        assert!(prog.transaction_activity.is_some());
    }
}

// ── statementBlock ───────────────────────────────────────────────────────
//   : statement nextStatement*
//   ;
mod statement_block {
    use super::*;

    /// statement  (no nextStatement)
    #[test]
    fn single_statement() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::Query(_)));
        assert!(b.next.is_empty());
    }

    /// statement nextStatement
    #[test]
    fn one_next() {
        let prog = p("MATCH (a) RETURN a NEXT MATCH (b) RETURN b");
        let b = body(&prog);
        assert_eq!(b.next.len(), 1);
    }

    /// statement nextStatement nextStatement
    #[test]
    fn two_next() {
        let prog = p("MATCH (a) RETURN a NEXT MATCH (b) RETURN b NEXT MATCH (c) RETURN c");
        let b = body(&prog);
        assert_eq!(b.next.len(), 2);
        assert_eq!(b.iter_statements().count(), 3);
    }
}

// ── statement ────────────────────────────────────────────────────────────
//   : compositeQueryStatement
//   | linearCatalogModifyingStatement
//   | linearDataModifyingStatement
//   ;
mod statement {
    use super::*;

    /// compositeQueryStatement
    #[test]
    fn composite_query() {
        let prog = p("MATCH (n) RETURN n");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::Query(_)));
    }

    /// linearCatalogModifyingStatement — CREATE SCHEMA
    #[test]
    fn catalog_modifying_create_schema() {
        let prog = p("CREATE SCHEMA /s");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::CreateSchema(_)));
    }

    /// linearCatalogModifyingStatement — DROP SCHEMA
    #[test]
    fn catalog_modifying_drop_schema() {
        let prog = p("DROP SCHEMA /s");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::DropSchema(_)));
    }

    /// linearCatalogModifyingStatement — CREATE GRAPH
    #[test]
    fn catalog_modifying_create_graph() {
        let prog = p("CREATE GRAPH g ANY");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::CreateGraph(_)));
    }

    /// linearCatalogModifyingStatement — DROP GRAPH
    #[test]
    fn catalog_modifying_drop_graph() {
        let prog = p("DROP GRAPH g");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::DropGraph(_)));
    }

    /// linearCatalogModifyingStatement — CREATE GRAPH TYPE
    #[test]
    fn catalog_modifying_create_graph_type() {
        let prog = p("CREATE GRAPH TYPE gt {}");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::CreateGraphType(_)));
    }

    /// linearCatalogModifyingStatement — DROP GRAPH TYPE
    #[test]
    fn catalog_modifying_drop_graph_type() {
        let prog = p("DROP GRAPH TYPE gt");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::DropGraphType(_)));
    }

    /// linearDataModifyingStatement — INSERT
    #[test]
    fn data_modifying_insert() {
        let prog = p("INSERT (:Person {name: 'Alice'})");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::Insert(_)));
    }

    /// linearDataModifyingStatement — SET (embedded in linear query)
    #[test]
    fn data_modifying_set() {
        let prog = p("MATCH (n) SET n.x = 1 RETURN n");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::Query(_)));
    }

    /// linearDataModifyingStatement — DELETE (embedded in linear query)
    #[test]
    fn data_modifying_delete() {
        let prog = p("MATCH (n) DELETE n");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::Query(_)));
    }

    /// linearDataModifyingStatement — REMOVE (embedded in linear query)
    #[test]
    fn data_modifying_remove() {
        let prog = p("MATCH (n) REMOVE n.x RETURN n");
        let b = body(&prog);
        assert!(matches!(b.first, Statement::Query(_)));
    }
}

// ── nextStatement ────────────────────────────────────────────────────────
//   : NEXT yieldClause? statement
//   ;
mod next_statement {
    use super::*;

    /// NEXT statement  (no yieldClause)
    #[test]
    fn next_without_yield() {
        let prog = p("MATCH (a) RETURN a NEXT MATCH (b) RETURN b");
        let b = body(&prog);
        assert_eq!(b.next.len(), 1);
        assert!(b.next[0].yield_items.is_none());
        assert!(matches!(b.next[0].statement, Statement::Query(_)));
    }

    /// NEXT yieldClause statement
    #[test]
    fn next_with_yield() {
        let prog = p("MATCH (n) RETURN n AS x NEXT YIELD x AS y MATCH (y) RETURN y");
        let b = body(&prog);
        assert_eq!(b.next.len(), 1);
        let yi = b.next[0].yield_items.as_ref().unwrap();
        assert_eq!(yi.len(), 1);
        assert_eq!(yi[0].alias.as_deref(), Some("y"));
    }

    /// NEXT yieldClause with multiple yield items
    #[test]
    fn next_with_yield_multiple_items() {
        let prog = p("MATCH (a)-[e]->(b) RETURN a AS x, b AS y \
             NEXT YIELD x, y AS z MATCH (z) RETURN z");
        let b = body(&prog);
        let yi = b.next[0].yield_items.as_ref().unwrap();
        assert_eq!(yi.len(), 2);
        assert!(yi[0].alias.is_none());
        assert_eq!(yi[1].alias.as_deref(), Some("z"));
    }
}
