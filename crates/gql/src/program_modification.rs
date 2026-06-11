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
}
