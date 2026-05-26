use crate::ast::*;
use rapidhash::RapidHashSet;

use super::expr::validate_expr;
use super::{VResult, verr};

pub(super) fn validate_insert(ins: &InsertStatement) -> VResult {
    if ins.patterns.is_empty() {
        return Err(verr("INSERT must have at least one pattern"));
    }
    Ok(())
}

pub(super) fn validate_set_items(items: &[SetItem]) -> VResult {
    if items.is_empty() {
        return Err(verr("SET must have at least one item"));
    }
    Ok(())
}

pub(super) fn validate_set_vars(
    items: &[SetItem],
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    validate_set_items(items)?;
    for item in items {
        match item {
            SetItem::Property {
                variable, value, ..
            } => {
                if !scope.contains(variable) {
                    return Err(verr(&format!(
                        "SET target variable '{variable}' is not bound in scope"
                    )));
                }
                validate_expr(value, scope, graph_scope)?;
            }
            SetItem::AllProperties {
                variable, value, ..
            } => {
                if !scope.contains(variable) {
                    return Err(verr(&format!(
                        "SET target variable '{variable}' is not bound in scope"
                    )));
                }
                validate_expr(value, scope, graph_scope)?;
            }
            SetItem::Label { variable, .. } => {
                if !scope.contains(variable) {
                    return Err(verr(&format!(
                        "SET target variable '{variable}' is not bound in scope"
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_remove_items(items: &[RemoveItem]) -> VResult {
    if items.is_empty() {
        return Err(verr("REMOVE must have at least one item"));
    }
    Ok(())
}

pub(super) fn validate_remove_vars(items: &[RemoveItem], scope: &RapidHashSet<String>) -> VResult {
    validate_remove_items(items)?;
    for item in items {
        let variable = match item {
            RemoveItem::Property { variable, .. } => variable,
            RemoveItem::Label { variable, .. } => variable,
        };
        if !scope.contains(variable) {
            return Err(verr(&format!(
                "REMOVE target variable '{variable}' is not bound in scope"
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_delete(del: &DeleteStatement) -> VResult {
    if del.items.is_empty() {
        return Err(verr("DELETE must have at least one target"));
    }
    Ok(())
}

pub(super) fn validate_delete_vars(
    del: &DeleteStatement,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    validate_delete(del)?;
    for item in &del.items {
        validate_expr(item, scope, graph_scope)?;
    }
    for item in &del.items {
        if let ExprKind::Variable(var) = &item.kind
            && !scope.contains(var)
        {
            return Err(verr(&format!(
                "DELETE target variable '{var}' is not bound in scope"
            )));
        }
    }
    Ok(())
}
