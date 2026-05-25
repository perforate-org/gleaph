use gleaph_gql::ast::ExprKind;

use crate::expr_children::for_each_immediate_child_expr;

/// Collect all variable references in an expression (cloning variant).
pub fn collect_variables(expr: &gleaph_gql::ast::Expr) -> Vec<String> {
    let mut vars = Vec::new();
    collect_variables_ref(expr, &mut |v| vars.push(v.to_string()));
    vars.sort();
    vars.dedup();
    vars
}

/// Walk expression tree and call `f` with each variable reference (zero-copy).
pub fn collect_variables_ref(expr: &gleaph_gql::ast::Expr, f: &mut impl FnMut(&str)) {
    if let ExprKind::Variable(v) = &expr.kind {
        f(v);
    }
    for_each_immediate_child_expr(expr, |child| collect_variables_ref(child, f));
}

/// Check if all variables in an expression equal a specific variable.
pub(crate) fn all_variables_eq(expr: &gleaph_gql::ast::Expr, target: &str) -> bool {
    let mut result = true;
    collect_variables_ref(expr, &mut |v| {
        if v != target {
            result = false;
        }
    });
    result
}
