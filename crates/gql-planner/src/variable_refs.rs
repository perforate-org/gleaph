//! Collect unqualified variable names referenced anywhere in a linear query (for demand-driven plan fields).
//!
//! Includes variables inside [`SimpleQueryStatement::InlineProcedureCall`] bodies (`CALL { ... }`).

use std::collections::BTreeSet;

use gleaph_gql::ast::*;

use crate::pushdown::collect_variables_ref;

/// All `Variable` names appearing in `query` (parts, result, prefix bindings, nested composite in bindings).
pub fn linear_query_referenced_variables(query: &LinearQueryStatement) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    linear_query_referenced_variables_into(query, &mut out);
    out
}

fn linear_query_referenced_variables_into(query: &LinearQueryStatement, out: &mut BTreeSet<String>) {
    for def in &query.prefix_bindings {
        match &def.initializer {
            ProcedureBindingInitializer::Expr(e) => add_expr_vars(e, out),
            ProcedureBindingInitializer::Query(boxed) => add_composite_vars(boxed.as_ref(), out),
            ProcedureBindingInitializer::Object(_) => {}
        }
    }
    for part in &query.parts {
        simple_statement_vars(part, out);
    }
    if let Some(result) = &query.result {
        result_statement_vars(result, out);
    }
}

fn add_composite_vars(c: &CompositeQueryExpr, out: &mut BTreeSet<String>) {
    linear_query_referenced_variables_into(&c.left, out);
    for (_, q) in &c.rest {
        linear_query_referenced_variables_into(q, out);
    }
}

fn add_expr_vars(expr: &Expr, out: &mut BTreeSet<String>) {
    collect_variables_ref(expr, &mut |v| {
        out.insert(v.to_string());
    });
}

fn simple_statement_vars(stmt: &SimpleQueryStatement, out: &mut BTreeSet<String>) {
    match stmt {
        SimpleQueryStatement::Match(m) => {
            for path in &m.pattern.paths {
                path_pattern_expr_vars(&path.expr, out);
            }
            if let Some(w) = &m.pattern.where_clause {
                add_expr_vars(w, out);
            }
        }
        SimpleQueryStatement::Filter(f) => add_expr_vars(&f.condition, out),
        SimpleQueryStatement::Let(l) => {
            for b in &l.bindings {
                add_expr_vars(&b.value, out);
            }
        }
        SimpleQueryStatement::For(f) => {
            add_expr_vars(&f.list, out);
        }
        SimpleQueryStatement::OrderBy(ob) => {
            for it in &ob.items {
                add_expr_vars(&it.expr, out);
            }
        }
        SimpleQueryStatement::Limit(l) => add_expr_vars(&l.count, out),
        SimpleQueryStatement::Offset(o) => add_expr_vars(&o.count, out),
        SimpleQueryStatement::CallProcedure(c) => {
            for a in &c.args {
                add_expr_vars(a, out);
            }
        }
        SimpleQueryStatement::InlineProcedureCall(ipc) => {
            add_composite_vars(ipc.body.as_ref(), out);
        }
        SimpleQueryStatement::Focused { body, .. } => {
            if let Some(b) = body {
                simple_statement_vars(b.as_ref(), out);
            }
        }
        SimpleQueryStatement::Insert(ins) => {
            for pat in &ins.patterns {
                for el in &pat.elements {
                    match el {
                        InsertElement::Node(n) => {
                            for p in &n.properties {
                                add_expr_vars(&p.value, out);
                            }
                        }
                        InsertElement::Edge(e) => {
                            for p in &e.properties {
                                add_expr_vars(&p.value, out);
                            }
                        }
                    }
                }
            }
        }
        SimpleQueryStatement::Set(s) => {
            for it in &s.items {
                match it {
                    SetItem::Property { value, .. } | SetItem::AllProperties { value, .. } => {
                        add_expr_vars(value, out);
                    }
                    SetItem::Label { .. } => {}
                }
            }
        }
        SimpleQueryStatement::Remove(_) => {}
        SimpleQueryStatement::Delete(d) => {
            for e in &d.items {
                add_expr_vars(e, out);
            }
        }
    }
}

fn path_pattern_expr_vars(expr: &PathPatternExpr, out: &mut BTreeSet<String>) {
    match expr {
        PathPatternExpr::Term(term) => {
            for factor in &term.factors {
                path_primary_vars(&factor.primary, out);
            }
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                for factor in &term.factors {
                    path_primary_vars(&factor.primary, out);
                }
            }
        }
    }
}

fn path_primary_vars(primary: &PathPrimary, out: &mut BTreeSet<String>) {
    match primary {
        PathPrimary::Node(node) => {
            for p in &node.properties {
                add_expr_vars(&p.value, out);
            }
            if let Some(w) = &node.where_clause {
                add_expr_vars(w, out);
            }
        }
        PathPrimary::Edge(edge) => {
            for p in &edge.properties {
                add_expr_vars(&p.value, out);
            }
            if let Some(w) = &edge.where_clause {
                add_expr_vars(w, out);
            }
        }
        PathPrimary::Parenthesized {
            expr, where_clause, ..
        } => {
            path_pattern_expr_vars(expr, out);
            if let Some(w) = where_clause {
                add_expr_vars(w, out);
            }
        }
        PathPrimary::Simplified(_) => {}
    }
}

fn result_statement_vars(result: &ResultStatement, out: &mut BTreeSet<String>) {
    match result {
        ResultStatement::Return(ret) => match &ret.body {
            ReturnBody::Star => {}
            #[cfg(feature = "cypher")]
            ReturnBody::NoBindings => {}
            ReturnBody::Items {
                items,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => {
                return_items_extras_vars(items, group_by, having, order_by, limit, offset, out);
            }
        },
        ResultStatement::Select(sel) => match &sel.body {
            SelectBody::Star {
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => {
                return_items_extras_vars(&[], group_by, having, order_by, limit, offset, out);
            }
            SelectBody::Items {
                items,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => {
                return_items_extras_vars(items, group_by, having, order_by, limit, offset, out);
            }
        },
        ResultStatement::Finish => {}
    }
}

fn return_items_extras_vars(
    items: &[ReturnItem],
    group_by: &Option<GroupByClause>,
    having: &Option<Expr>,
    order_by: &Option<OrderByClause>,
    limit: &Option<LimitClause>,
    offset: &Option<OffsetClause>,
    out: &mut BTreeSet<String>,
) {
    for item in items {
        add_expr_vars(&item.expr, out);
    }
    if let Some(gb) = group_by {
        for e in &gb.items {
            add_expr_vars(e, out);
        }
    }
    if let Some(h) = having {
        add_expr_vars(h, out);
    }
    if let Some(ob) = order_by {
        for it in &ob.items {
            add_expr_vars(&it.expr, out);
        }
    }
    if let Some(l) = limit {
        add_expr_vars(&l.count, out);
    }
    if let Some(o) = offset {
        add_expr_vars(&o.count, out);
    }
}

#[cfg(test)]
mod tests {
    use gleaph_gql::ast::{LinearQueryStatement, Statement};
    use gleaph_gql::parser;

    use super::linear_query_referenced_variables;

    fn linear_query_from_str(input: &str) -> LinearQueryStatement {
        let program = parser::parse(input).expect("parse");
        let tx = program.transaction_activity.expect("transaction_activity");
        let block = tx.body.expect("block body");
        match &block.first {
            Statement::Query(composite) => composite.left.clone(),
            other => panic!("expected Query statement, got {other:?}"),
        }
    }

    #[test]
    fn inline_procedure_call_body_contributes_referenced_variables() {
        let q = linear_query_from_str(
            "MATCH (x:Person) CALL { MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN e__hop_aux } RETURN x",
        );
        let vars = linear_query_referenced_variables(&q);
        assert!(
            vars.contains("e__hop_aux"),
            "inner RETURN e__hop_aux should be collected, got {vars:?}"
        );
    }
}
