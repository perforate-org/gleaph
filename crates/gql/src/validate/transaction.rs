use rapidhash::RapidHashSet;

use crate::ast::*;

use super::{
    VResult, session_graph, statement_result_scopes, validate_statement_with_scope,
    validate_yield_alias_uniqueness, verr,
};

pub(super) fn validate_transaction_activity(ta: &TransactionActivity) -> VResult {
    if let Some(ref start) = ta.start {
        validate_start_transaction(start)?;
    }

    if let Some(ref body) = ta.body {
        let mut scope = RapidHashSet::default();
        let mut graph_scope = session_graph::initial_graph_scope();
        validate_statement_with_scope(&body.first, &scope, &graph_scope)?;
        let (mut prev_result_scope, mut prev_result_graph_scope) =
            statement_result_scopes(&body.first, &scope, &graph_scope)?;
        for next in &body.next {
            if let Some(ref yields) = next.yield_items {
                validate_yield_alias_uniqueness(yields, "NEXT YIELD")?;
                let mut projected = RapidHashSet::default();
                let mut projected_graph = RapidHashSet::default();
                for yi in yields {
                    if !prev_result_scope.contains(&yi.name) {
                        return Err(verr(&format!(
                            "NEXT YIELD variable '{}' is not in scope",
                            yi.name
                        )));
                    }
                    let output_name = yi.alias.clone().unwrap_or_else(|| yi.name.clone());
                    if prev_result_graph_scope.contains(&yi.name) {
                        projected_graph.insert(output_name.clone());
                    }
                    projected.insert(output_name);
                }
                scope = projected;
                graph_scope = projected_graph;
            } else {
                scope = prev_result_scope.clone();
                graph_scope = prev_result_graph_scope.clone();
            }
            validate_statement_with_scope(&next.statement, &scope, &graph_scope)?;
            (prev_result_scope, prev_result_graph_scope) =
                statement_result_scopes(&next.statement, &scope, &graph_scope)?;
        }
    }

    Ok(())
}

fn validate_start_transaction(start: &StartTransactionCommand) -> VResult {
    let mut has_read_only = false;
    let mut has_read_write = false;
    for mode in &start.access_modes {
        match mode {
            TransactionAccessMode::ReadOnly => has_read_only = true,
            TransactionAccessMode::ReadWrite => has_read_write = true,
        }
    }
    if has_read_only && has_read_write {
        return Err(verr(
            "START TRANSACTION has contradictory access modes: both READ ONLY and READ WRITE",
        ));
    }
    Ok(())
}
