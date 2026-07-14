//! Classify a parsed GQL program for data-modifying and catalog-modifying content.
//!
//! Used by host authorization policies: [`Role::Read`] rejects programs where
//! [`ProgramModificationFlags::requires_write_path`] is true.

use crate::ast::{
    CompositeQueryExpr, GqlProgram, LinearQueryStatement, ProcedureBindingInitializer,
    SimpleQueryStatement, Statement, StatementBlock,
};

/// Booleans derived from a static AST walk (conservative rules for unknown procedure calls).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgramModificationFlags {
    /// DML: `INSERT`, `SET`, `REMOVE`, `DELETE` (top-level or inside a linear query / inline procedure).
    pub has_data_modification: bool,
    /// DDL: `CREATE`/`DROP` schema, graph, graph type (GQL §12).
    pub has_catalog_modification: bool,
    /// Named `CALL` procedure (semantics unknown — treated as requiring a write-capable caller).
    pub has_call_procedure: bool,
}

impl ProgramModificationFlags {
    /// [`Role::Read`] may execute only when this is false.
    pub fn requires_write_path(self) -> bool {
        self.has_data_modification || self.has_catalog_modification || self.has_call_procedure
    }
}

/// Inspect a parsed program (after successful parse).
pub fn classify_program(program: &GqlProgram) -> ProgramModificationFlags {
    let _ = &program.session_activity;
    if let Some(tx) = &program.transaction_activity
        && let Some(body) = &tx.body
    {
        return classify_statement_block(body);
    }
    ProgramModificationFlags::default()
}

/// Classify only a transaction [`StatementBlock`] (same rules as [`classify_program`] for typical TX bodies).
pub fn classify_statement_block(block: &StatementBlock) -> ProgramModificationFlags {
    let mut flags = ProgramModificationFlags::default();
    walk_statement_block(block, &mut flags);
    flags
}

/// Count top-level block statements that perform data modification (DML).
///
/// The unit is the top-level statement (`StatementBlock::first` plus each chained `NEXT`
/// statement). A single top-level statement counts once regardless of how many DML parts it
/// contains, e.g. `MATCH (n) SET n.a = 1 SET n.b = 2` is one statement. DDL-only and read-only
/// statements do not count. Used by the federated multi-DML bundle gate (ADR 0029 Phase 5).
pub fn count_dml_statements(block: &StatementBlock) -> usize {
    block
        .iter_statements()
        .filter(|st| statement_has_data_modification(st))
        .count()
}

fn statement_has_data_modification(stmt: &Statement) -> bool {
    let mut flags = ProgramModificationFlags::default();
    walk_statement(stmt, &mut flags);
    flags.has_data_modification
}

fn walk_statement_block(block: &StatementBlock, flags: &mut ProgramModificationFlags) {
    for st in block.iter_statements() {
        walk_statement(st, flags);
    }
}

fn walk_statement(stmt: &Statement, flags: &mut ProgramModificationFlags) {
    match stmt {
        Statement::Insert(_) | Statement::Set(_) | Statement::Remove(_) | Statement::Delete(_) => {
            flags.has_data_modification = true;
        }
        Statement::CreateSchema(_)
        | Statement::DropSchema(_)
        | Statement::CreateGraph(_)
        | Statement::DropGraph(_)
        | Statement::CreateGraphType(_)
        | Statement::DropGraphType(_) => {
            flags.has_catalog_modification = true;
        }
        Statement::Query(q) => walk_composite(q, flags),
        Statement::Session(_) => {}
    }
}

fn walk_composite(expr: &CompositeQueryExpr, flags: &mut ProgramModificationFlags) {
    walk_linear(&expr.left, flags);
    for (_, lq) in &expr.rest {
        walk_linear(lq, flags);
    }
}

fn walk_linear(lq: &LinearQueryStatement, flags: &mut ProgramModificationFlags) {
    for b in &lq.prefix_bindings {
        match &b.initializer {
            ProcedureBindingInitializer::Query(q) => walk_composite(q, flags),
            ProcedureBindingInitializer::Object(_) | ProcedureBindingInitializer::Expr(_) => {}
        }
    }
    for part in &lq.parts {
        walk_simple_part(part, flags);
    }
}

fn walk_simple_part(part: &SimpleQueryStatement, flags: &mut ProgramModificationFlags) {
    match part {
        SimpleQueryStatement::Insert(_)
        | SimpleQueryStatement::Set(_)
        | SimpleQueryStatement::Remove(_)
        | SimpleQueryStatement::Delete(_) => {
            flags.has_data_modification = true;
        }
        SimpleQueryStatement::CallProcedure(_) => {
            flags.has_call_procedure = true;
        }
        SimpleQueryStatement::InlineProcedureCall(ipc) => {
            walk_composite(&ipc.body, flags);
        }
        SimpleQueryStatement::Focused { body, .. } => {
            if let Some(b) = body {
                walk_simple_part(b, flags);
            }
        }
        SimpleQueryStatement::Match(_)
        | SimpleQueryStatement::Filter(_)
        | SimpleQueryStatement::Let(_)
        | SimpleQueryStatement::For(_)
        | SimpleQueryStatement::OrderBy(_)
        | SimpleQueryStatement::Limit(_)
        | SimpleQueryStatement::Offset(_) => {}
        #[cfg(feature = "gleaph")]
        SimpleQueryStatement::Search(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn match_only_is_read_safe() {
        let p = parser::parse("MATCH (n) RETURN n").expect("parse");
        let f = classify_program(&p);
        assert!(!f.requires_write_path());
    }

    #[test]
    fn insert_top_level_requires_write() {
        let p = parser::parse("INSERT (n:Person {age: 42})").expect("parse");
        let f = classify_program(&p);
        assert!(f.has_data_modification);
        assert!(f.requires_write_path());
    }

    #[test]
    fn classify_statement_block_matches_program_tx_body() {
        let p = parser::parse("MATCH (n) RETURN n UNION MATCH (m) RETURN m").expect("parse");
        let body = p
            .transaction_activity
            .as_ref()
            .expect("tx")
            .body
            .as_ref()
            .expect("body");
        assert_eq!(classify_program(&p), classify_statement_block(body));
    }

    #[test]
    fn create_graph_requires_write() {
        let p = parser::parse("CREATE GRAPH g").expect("parse");
        let f = classify_program(&p);
        assert!(f.has_catalog_modification);
        assert!(f.requires_write_path());
    }

    fn dml_statement_count(query: &str) -> usize {
        let p = parser::parse(query).expect("parse");
        let body = p
            .transaction_activity
            .as_ref()
            .expect("tx")
            .body
            .as_ref()
            .expect("body");
        count_dml_statements(body)
    }

    #[test]
    fn count_dml_statements_zero_for_read_only() {
        assert_eq!(dml_statement_count("MATCH (n) RETURN n"), 0);
    }

    #[test]
    fn count_dml_statements_one_for_single_insert() {
        assert_eq!(dml_statement_count("INSERT (n:Person {age: 42})"), 1);
    }

    #[test]
    fn count_dml_statements_one_for_single_statement_with_multiple_dml_parts() {
        // One top-level statement (one linear query) counts once even with several DML parts.
        assert_eq!(dml_statement_count("MATCH (n) SET n.a = 1 SET n.b = 2"), 1);
    }

    #[test]
    fn count_dml_statements_counts_each_next_chained_dml_statement() {
        assert_eq!(dml_statement_count("INSERT (a:A) NEXT INSERT (b:B)"), 2);
    }

    #[test]
    fn count_dml_statements_ignores_read_only_next_statements() {
        assert_eq!(
            dml_statement_count("INSERT (a:A) NEXT MATCH (n) RETURN n"),
            1
        );
    }
}
