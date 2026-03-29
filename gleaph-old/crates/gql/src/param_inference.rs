//! Reverse parameter type inference for prepared statements.
//!
//! Walks the AST to infer types for unannotated `$param` expressions from
//! their usage context (comparisons, arithmetic, string predicates).
//!
//! This is a **metadata-only** pass — it does not change runtime semantics.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::semantic::PropertySchema;

/// Result of parameter type inference for a single parameter.
#[derive(Clone, Debug)]
pub struct InferredParam {
    /// Inferred or explicit types.
    pub types: Vec<ValueType>,
    /// `true` when the annotation came from the AST (`$x :: INT`).
    pub explicit: bool,
    /// Whether the parameter is required (no `| NULL` in annotation).
    pub required: bool,
    /// Conflict messages when evidence disagrees.
    pub conflicts: Vec<String>,
}

/// Infer parameter types for all `$param` references in a statement.
///
/// 1. Collects all parameter occurrences with explicit annotations.
/// 2. Walks comparison / predicate sites and reverse-infers from the other operand.
pub fn infer_parameter_types(
    stmt: &Statement,
    schema: &dyn PropertySchema,
) -> BTreeMap<String, InferredParam> {
    let mut params: BTreeMap<String, InferredParam> = BTreeMap::new();

    // Phase 1: collect all parameter occurrences and explicit annotations.
    collect_params_from_stmt(stmt, &mut params);

    // Phase 2: build variable → label map and reverse-infer from typed usage.
    let mut var_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut var_edge_label: BTreeMap<String, String> = BTreeMap::new();
    collect_var_labels_from_stmt(stmt, &mut var_labels, &mut var_edge_label);

    let ctx = InferCtx {
        schema,
        var_labels,
        var_edge_label,
    };
    infer_from_stmt(stmt, &ctx, &mut params);

    // Suppress types on unresolved conflicts.
    for param in params.values_mut() {
        if !param.conflicts.is_empty() && !param.explicit {
            param.types.clear();
        }
    }

    params
}

// ---------------------------------------------------------------------------
// Phase 1: collect explicit annotations (delegates expr walking)
// ---------------------------------------------------------------------------

fn collect_params_from_stmt(stmt: &Statement, out: &mut BTreeMap<String, InferredParam>) {
    // We reuse the same AST walk shape as executor::collect_prepared_parameter_info_from_stmt
    // but store richer data per parameter.
    match stmt {
        Statement::Query(q) => collect_params_from_query(q, out),
        Statement::Compound { left, right, .. } => {
            collect_params_from_stmt(left, out);
            collect_params_from_stmt(right, out);
        }
        Statement::Create(cs) => {
            for c in cs {
                collect_params_from_create(c, out);
            }
        }
        Statement::Merge(ms) => {
            collect_params_from_create(&ms.create, out);
            for item in &ms.on_create_set {
                collect_params_from_set_item(item, out);
            }
            for item in &ms.on_match_set {
                collect_params_from_set_item(item, out);
            }
        }
        Statement::Delete(ds) => {
            collect_params_from_match_clause(&ds.match_clause, out);
            if let Some(w) = &ds.where_clause {
                collect_params_from_expr(w, out);
            }
        }
        Statement::Set(ss) => {
            collect_params_from_match_clause(&ss.match_clause, out);
            if let Some(w) = &ss.where_clause {
                collect_params_from_expr(w, out);
            }
            for item in &ss.set_clause.items {
                collect_params_from_set_item(item, out);
            }
        }
        Statement::Remove(rs) => {
            collect_params_from_match_clause(&rs.match_clause, out);
            if let Some(w) = &rs.where_clause {
                collect_params_from_expr(w, out);
            }
        }
        Statement::Filter(fs) => {
            collect_params_from_match_clause(&fs.match_clause, out);
            if let Some(w) = &fs.where_clause {
                collect_params_from_expr(w, out);
            }
            collect_params_from_expr(&fs.filter_expr, out);
        }
        Statement::Let(ls) => {
            collect_params_from_match_clause(&ls.match_clause, out);
            if let Some(w) = &ls.where_clause {
                collect_params_from_expr(w, out);
            }
            for (_, e) in &ls.bindings {
                collect_params_from_expr(e, out);
            }
            collect_params_from_return(&ls.return_clause, out);
        }
        Statement::For(fs) => {
            collect_params_from_expr(&fs.list_expr, out);
            collect_params_from_return(&fs.return_clause, out);
        }
        Statement::Call(cs) => collect_params_from_stmt(&cs.body, out),
        _ => {}
    }
}

fn collect_params_from_query(q: &QueryStmt, out: &mut BTreeMap<String, InferredParam>) {
    for me in &q.match_clauses {
        collect_params_from_match_clause(&me.pattern, out);
    }
    if let Some(w) = &q.where_clause {
        collect_params_from_expr(w, out);
    }
    for wc in &q.with_clauses {
        for item in &wc.items {
            collect_params_from_expr(&item.expr, out);
        }
        if let Some(w) = &wc.where_clause {
            collect_params_from_expr(w, out);
        }
        for me in &wc.match_clauses {
            collect_params_from_match_clause(&me.pattern, out);
        }
        if let Some(w) = &wc.post_match_where {
            collect_params_from_expr(w, out);
        }
    }
    collect_params_from_return(&q.return_clause, out);
    if let Some(h) = &q.having {
        collect_params_from_expr(h, out);
    }
}

fn collect_params_from_return(r: &ReturnClause, out: &mut BTreeMap<String, InferredParam>) {
    for item in &r.items {
        collect_params_from_expr(&item.expr, out);
    }
}

fn collect_params_from_match_clause(mc: &MatchClause, out: &mut BTreeMap<String, InferredParam>) {
    for (_, e) in &mc.start.props_hint {
        collect_params_from_expr(e, out);
    }
    if let Some(w) = &mc.start.where_clause {
        collect_params_from_expr(w, out);
    }
    for pe in &mc.elements {
        if let PatternElement::Hop(c) = pe {
            for (_, e) in &c.edge.properties {
                collect_params_from_expr(e, out);
            }
            for (_, e) in &c.node.props_hint {
                collect_params_from_expr(e, out);
            }
        }
    }
}

fn collect_params_from_create(cs: &CreateStmt, out: &mut BTreeMap<String, InferredParam>) {
    match cs {
        CreateStmt::Node(nc) => {
            for (_, e) in &nc.node.props_hint {
                collect_params_from_expr(e, out);
            }
        }
        CreateStmt::Edge(ec) => {
            for (_, e) in &ec.left.props_hint {
                collect_params_from_expr(e, out);
            }
            for (_, e) in &ec.edge.properties {
                collect_params_from_expr(e, out);
            }
            for (_, e) in &ec.right.props_hint {
                collect_params_from_expr(e, out);
            }
        }
    }
}

fn collect_params_from_set_item(item: &SetItem, out: &mut BTreeMap<String, InferredParam>) {
    if let SetItem::Property { value, .. } = item {
        collect_params_from_expr(value, out);
    }
}

fn collect_params_from_expr(expr: &Expr, out: &mut BTreeMap<String, InferredParam>) {
    match expr {
        Expr::Parameter {
            name,
            type_annotation,
        } => {
            let allows_null = type_annotation
                .as_ref()
                .is_some_and(|types| types.contains(&ValueType::Null));
            let required = !allows_null;
            let (types, explicit) = match type_annotation {
                Some(types) => (types.clone(), true),
                None => (vec![], false),
            };
            out.entry(name.clone())
                .and_modify(|existing| {
                    if explicit && !existing.explicit {
                        existing.types = types.clone();
                        existing.explicit = true;
                    }
                    existing.required = existing.required || required;
                })
                .or_insert(InferredParam {
                    types,
                    explicit,
                    required,
                    conflicts: vec![],
                });
        }
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => {
            collect_params_from_expr(left, out);
            collect_params_from_expr(right, out);
        }
        Expr::UnaryOp { expr: e, .. }
        | Expr::Not(e)
        | Expr::IsNull(e)
        | Expr::IsNotNull(e)
        | Expr::PathLength(e)
        | Expr::PropertyAccess { target: e, .. }
        | Expr::IsLabeled { expr: e, .. }
        | Expr::IsTruth { expr: e, .. }
        | Expr::Cast { expr: e, .. }
        | Expr::IsType { expr: e, .. }
        | Expr::IsDirected { expr: e, .. }
        | Expr::PropertyExists { target: e, .. } => {
            collect_params_from_expr(e, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_params_from_expr(expr, out);
            for item in list {
                collect_params_from_expr(item, out);
            }
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            collect_params_from_expr(expr, out);
            collect_params_from_expr(pattern, out);
        }
        Expr::Case(c) => {
            if let Some(op) = &c.operand {
                collect_params_from_expr(op, out);
            }
            for wt in &c.when_then {
                collect_params_from_expr(&wt.when, out);
                collect_params_from_expr(&wt.then, out);
            }
            if let Some(el) = &c.else_expr {
                collect_params_from_expr(el, out);
            }
        }
        Expr::Coalesce(items)
        | Expr::ListLiteral(items)
        | Expr::AllDifferent(items)
        | Expr::Same(items)
        | Expr::PathConstructor(items) => {
            for item in items {
                collect_params_from_expr(item, out);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_params_from_expr(arg, out);
            }
        }
        Expr::Aggregate(agg) => {
            if let Some(e) = &agg.expr {
                collect_params_from_expr(e, out);
            }
            if let Some(sep) = &agg.separator {
                collect_params_from_expr(sep, out);
            }
        }
        Expr::RecordLiteral(fields) => {
            for (_, v) in fields {
                collect_params_from_expr(v, out);
            }
        }
        Expr::LetIn { bindings, body } => {
            for (_, e) in bindings {
                collect_params_from_expr(e, out);
            }
            collect_params_from_expr(body, out);
        }
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            collect_params_from_expr(node, out);
            collect_params_from_expr(edge, out);
        }
        Expr::Exists(s) | Expr::ValueSubquery(s) => {
            collect_params_from_stmt(s, out);
        }
        Expr::Literal(_) | Expr::Variable(_) | Expr::PathVar(_) => {}
    }
}

// ---------------------------------------------------------------------------
// Variable → label mapping from MATCH patterns
// ---------------------------------------------------------------------------

fn collect_var_labels_from_stmt(
    stmt: &Statement,
    var_labels: &mut BTreeMap<String, Vec<String>>,
    var_edge_label: &mut BTreeMap<String, String>,
) {
    match stmt {
        Statement::Query(q) => {
            for me in &q.match_clauses {
                collect_var_labels_from_match_clause(&me.pattern, var_labels, var_edge_label);
            }
            for wc in &q.with_clauses {
                for me in &wc.match_clauses {
                    collect_var_labels_from_match_clause(&me.pattern, var_labels, var_edge_label);
                }
            }
        }
        Statement::Compound { left, right, .. } => {
            collect_var_labels_from_stmt(left, var_labels, var_edge_label);
            collect_var_labels_from_stmt(right, var_labels, var_edge_label);
        }
        Statement::Delete(ds) => {
            collect_var_labels_from_match_clause(&ds.match_clause, var_labels, var_edge_label)
        }
        Statement::Set(ss) => {
            collect_var_labels_from_match_clause(&ss.match_clause, var_labels, var_edge_label)
        }
        Statement::Remove(rs) => {
            collect_var_labels_from_match_clause(&rs.match_clause, var_labels, var_edge_label)
        }
        Statement::Filter(fs) => {
            collect_var_labels_from_match_clause(&fs.match_clause, var_labels, var_edge_label)
        }
        Statement::Let(ls) => {
            collect_var_labels_from_match_clause(&ls.match_clause, var_labels, var_edge_label)
        }
        Statement::Merge(ms) => {
            if let CreateStmt::Node(nc) = &ms.create
                && let Some(name) = &nc.node.var
            {
                let labels = node_labels(&nc.node);
                if !labels.is_empty() {
                    var_labels.insert(name.clone(), labels);
                }
            }
        }
        Statement::Call(cs) => collect_var_labels_from_stmt(&cs.body, var_labels, var_edge_label),
        _ => {}
    }
}

fn collect_var_labels_from_match_clause(
    mc: &MatchClause,
    var_labels: &mut BTreeMap<String, Vec<String>>,
    var_edge_label: &mut BTreeMap<String, String>,
) {
    // Start node
    if let Some(name) = &mc.start.var {
        let labels = node_labels(&mc.start);
        if !labels.is_empty() {
            var_labels.insert(name.clone(), labels);
        }
    }
    // Hops
    for pe in &mc.elements {
        if let PatternElement::Hop(c) = pe {
            if let Some(name) = &c.node.var {
                let labels = node_labels(&c.node);
                if !labels.is_empty() {
                    var_labels.insert(name.clone(), labels);
                }
            }
            if let Some(name) = &c.edge.var
                && let Some(label) = edge_single_label(&c.edge)
            {
                var_edge_label.insert(name.clone(), label);
            }
        }
    }
}

/// Extract labels from a NodePattern, falling back to `labels` when `label_expr` is None.
fn node_labels(node: &NodePattern) -> Vec<String> {
    let from_expr = extract_labels_from_label_expr(&node.label_expr);
    if !from_expr.is_empty() {
        from_expr
    } else {
        node.labels.clone()
    }
}

/// Extract a single edge label from an EdgePattern.
fn edge_single_label(edge: &EdgePattern) -> Option<String> {
    if let Some(LabelExpr::Name(label)) = &edge.label_expr {
        return Some(label.clone());
    }
    edge.label.clone()
}

fn extract_labels_from_label_expr(label_expr: &Option<LabelExpr>) -> Vec<String> {
    match label_expr {
        Some(le) => collect_label_names(le),
        None => vec![],
    }
}

/// Recursively collect all `Name` labels from a `LabelExpr` tree.
fn collect_label_names(le: &LabelExpr) -> Vec<String> {
    match le {
        LabelExpr::Name(name) => vec![name.clone()],
        LabelExpr::And(a, b) | LabelExpr::Or(a, b) => {
            let mut names = collect_label_names(a);
            names.extend(collect_label_names(b));
            names
        }
        LabelExpr::Not(_) | LabelExpr::Wildcard => vec![],
    }
}

// ---------------------------------------------------------------------------
// Phase 2: reverse inference from typed usage sites
// ---------------------------------------------------------------------------

struct InferCtx<'a> {
    schema: &'a dyn PropertySchema,
    var_labels: BTreeMap<String, Vec<String>>,
    var_edge_label: BTreeMap<String, String>,
}

impl InferCtx<'_> {
    fn property_type_for(&self, var: &str, prop: &str) -> Option<ValueType> {
        if let Some(labels) = self.var_labels.get(var) {
            for (name, vt, _) in self.schema.node_property_types(labels) {
                if name == prop {
                    return Some(vt);
                }
            }
        }
        if let Some(edge_label) = self.var_edge_label.get(var) {
            for (name, vt, _) in self.schema.edge_property_types(edge_label) {
                if name == prop {
                    return Some(vt);
                }
            }
        }
        None
    }
}

fn infer_from_stmt(
    stmt: &Statement,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    match stmt {
        Statement::Query(q) => infer_from_query(q, ctx, params),
        Statement::Compound { left, right, .. } => {
            infer_from_stmt(left, ctx, params);
            infer_from_stmt(right, ctx, params);
        }
        Statement::Create(cs) => {
            for c in cs {
                infer_from_create(c, ctx, params);
            }
        }
        Statement::Merge(ms) => {
            infer_from_create(&ms.create, ctx, params);
            for item in &ms.on_create_set {
                infer_from_set_item(item, ctx, params);
            }
            for item in &ms.on_match_set {
                infer_from_set_item(item, ctx, params);
            }
        }
        Statement::Delete(ds) => {
            infer_from_match_clause(&ds.match_clause, ctx, params);
            if let Some(w) = &ds.where_clause {
                infer_from_expr(w, ctx, params);
            }
        }
        Statement::Set(ss) => {
            infer_from_match_clause(&ss.match_clause, ctx, params);
            if let Some(w) = &ss.where_clause {
                infer_from_expr(w, ctx, params);
            }
            for item in &ss.set_clause.items {
                infer_from_set_item(item, ctx, params);
            }
        }
        Statement::Remove(rs) => {
            infer_from_match_clause(&rs.match_clause, ctx, params);
            if let Some(w) = &rs.where_clause {
                infer_from_expr(w, ctx, params);
            }
        }
        Statement::Filter(fs) => {
            infer_from_match_clause(&fs.match_clause, ctx, params);
            if let Some(w) = &fs.where_clause {
                infer_from_expr(w, ctx, params);
            }
            infer_from_expr(&fs.filter_expr, ctx, params);
        }
        Statement::Let(ls) => {
            infer_from_match_clause(&ls.match_clause, ctx, params);
            if let Some(w) = &ls.where_clause {
                infer_from_expr(w, ctx, params);
            }
            for (_, e) in &ls.bindings {
                infer_from_expr(e, ctx, params);
            }
            for item in &ls.return_clause.items {
                infer_from_expr(&item.expr, ctx, params);
            }
        }
        Statement::For(fs) => {
            infer_from_expr(&fs.list_expr, ctx, params);
            for item in &fs.return_clause.items {
                infer_from_expr(&item.expr, ctx, params);
            }
        }
        Statement::Call(cs) => infer_from_stmt(&cs.body, ctx, params),
        _ => {}
    }
}

fn infer_from_query(
    q: &QueryStmt,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    for me in &q.match_clauses {
        infer_from_match_clause(&me.pattern, ctx, params);
    }
    if let Some(w) = &q.where_clause {
        infer_from_expr(w, ctx, params);
    }
    for wc in &q.with_clauses {
        for item in &wc.items {
            infer_from_expr(&item.expr, ctx, params);
        }
        if let Some(w) = &wc.where_clause {
            infer_from_expr(w, ctx, params);
        }
        for me in &wc.match_clauses {
            infer_from_match_clause(&me.pattern, ctx, params);
        }
        if let Some(w) = &wc.post_match_where {
            infer_from_expr(w, ctx, params);
        }
    }
    for item in &q.return_clause.items {
        infer_from_expr(&item.expr, ctx, params);
    }
    if let Some(h) = &q.having {
        infer_from_expr(h, ctx, params);
    }
}

fn infer_from_match_clause(
    mc: &MatchClause,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    // Inline properties like (:User {age: $age}) — infer from schema
    infer_from_node_props(&mc.start, ctx, params);
    for pe in &mc.elements {
        if let PatternElement::Hop(c) = pe {
            infer_from_node_props(&c.node, ctx, params);
            infer_from_edge_props(&c.edge, ctx, params);
        }
    }
}

fn infer_from_node_props(
    node: &NodePattern,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    let labels = node_labels(node);
    if labels.is_empty() {
        return;
    }
    let schema_props = ctx.schema.node_property_types(&labels);
    for (prop_name, prop_value) in &node.props_hint {
        if let Expr::Parameter {
            name,
            type_annotation: None,
        } = prop_value
            && let Some((_, vt, _)) = schema_props.iter().find(|(n, _, _)| n == prop_name)
        {
            merge_inferred(params, name, *vt);
        }
    }
}

fn infer_from_edge_props(
    edge: &EdgePattern,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    let label = edge_single_label(edge);
    let edge_label = match &label {
        Some(l) => l.as_str(),
        None => return,
    };
    let schema_props = ctx.schema.edge_property_types(edge_label);
    for (prop_name, prop_value) in &edge.properties {
        if let Expr::Parameter {
            name,
            type_annotation: None,
        } = prop_value
            && let Some((_, vt, _)) = schema_props.iter().find(|(n, _, _)| n == prop_name)
        {
            merge_inferred(params, name, *vt);
        }
    }
}

fn infer_from_create(
    cs: &CreateStmt,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    match cs {
        CreateStmt::Node(nc) => infer_from_node_props(&nc.node, ctx, params),
        CreateStmt::Edge(ec) => {
            infer_from_node_props(&ec.left, ctx, params);
            infer_from_edge_props(&ec.edge, ctx, params);
            infer_from_node_props(&ec.right, ctx, params);
        }
    }
}

fn infer_from_set_item(
    item: &SetItem,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    if let SetItem::Property { value, .. } = item {
        infer_from_expr(value, ctx, params);
    }
}

fn infer_from_expr(expr: &Expr, ctx: &InferCtx<'_>, params: &mut BTreeMap<String, InferredParam>) {
    match expr {
        // Rule B: reverse-infer from comparisons
        Expr::Compare { left, right, .. } => {
            try_infer_from_pair(left, right, ctx, params);
            try_infer_from_pair(right, left, ctx, params);
            infer_from_expr(left, ctx, params);
            infer_from_expr(right, ctx, params);
        }
        // Rule C: reverse-infer from arithmetic
        Expr::BinaryOp {
            left, right, op, ..
        } => {
            if is_numeric_op(op) {
                try_infer_numeric(left, right, ctx, params);
                try_infer_numeric(right, left, ctx, params);
            }
            infer_from_expr(left, ctx, params);
            infer_from_expr(right, ctx, params);
        }
        // Rule D: string predicates → TEXT
        Expr::StringPredicate {
            expr: e, pattern, ..
        } => {
            if let Expr::Parameter {
                name,
                type_annotation: None,
            } = pattern.as_ref()
            {
                merge_inferred(params, name, ValueType::Text);
            }
            if let Expr::Parameter {
                name,
                type_annotation: None,
            } = e.as_ref()
            {
                merge_inferred(params, name, ValueType::Text);
            }
            infer_from_expr(e, ctx, params);
            infer_from_expr(pattern, ctx, params);
        }
        // Concat → TEXT
        Expr::Concat(left, right) => {
            if let Expr::Parameter {
                name,
                type_annotation: None,
            } = left.as_ref()
            {
                merge_inferred(params, name, ValueType::Text);
            }
            if let Expr::Parameter {
                name,
                type_annotation: None,
            } = right.as_ref()
            {
                merge_inferred(params, name, ValueType::Text);
            }
            infer_from_expr(left, ctx, params);
            infer_from_expr(right, ctx, params);
        }
        // IN list: if the tested expr is typed, infer list item types
        Expr::InList { expr: e, list, .. } => {
            let expr_type = resolve_expr_type(e, ctx);
            if let Some(vt) = expr_type {
                for item in list {
                    if let Expr::Parameter {
                        name,
                        type_annotation: None,
                    } = item
                    {
                        // `x.prop IN $param` where $param IS the list →
                        // infer TypedList(prop_type).
                        // `x.prop IN [.., $param, ..]` where $param is a list element →
                        // infer prop_type directly.
                        if list.len() == 1 {
                            if let Some(scalar) = ScalarType::from_value_type(vt) {
                                merge_inferred(params, name, ValueType::TypedList(scalar));
                            } else {
                                merge_inferred(params, name, ValueType::List);
                            }
                        } else {
                            merge_inferred(params, name, vt);
                        }
                    }
                }
            }
            // Reverse: if a list item is typed and the expr is a param
            if let Expr::Parameter {
                name,
                type_annotation: None,
            } = e.as_ref()
            {
                for item in list {
                    if let Some(vt) = resolve_expr_type(item, ctx) {
                        merge_inferred(params, name, vt);
                        break;
                    }
                }
            }
            infer_from_expr(e, ctx, params);
            for item in list {
                infer_from_expr(item, ctx, params);
            }
        }
        // Recurse into sub-expressions
        Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) | Expr::NullIf { left: l, right: r } => {
            infer_from_expr(l, ctx, params);
            infer_from_expr(r, ctx, params);
        }
        Expr::Not(e)
        | Expr::IsNull(e)
        | Expr::IsNotNull(e)
        | Expr::UnaryOp { expr: e, .. }
        | Expr::PathLength(e)
        | Expr::PropertyAccess { target: e, .. }
        | Expr::IsLabeled { expr: e, .. }
        | Expr::IsTruth { expr: e, .. }
        | Expr::Cast { expr: e, .. }
        | Expr::IsType { expr: e, .. }
        | Expr::IsDirected { expr: e, .. }
        | Expr::PropertyExists { target: e, .. } => {
            infer_from_expr(e, ctx, params);
        }
        Expr::Case(c) => {
            if let Some(op) = &c.operand {
                infer_from_expr(op, ctx, params);
            }
            for wt in &c.when_then {
                infer_from_expr(&wt.when, ctx, params);
                infer_from_expr(&wt.then, ctx, params);
            }
            if let Some(el) = &c.else_expr {
                infer_from_expr(el, ctx, params);
            }
        }
        Expr::Coalesce(items)
        | Expr::ListLiteral(items)
        | Expr::AllDifferent(items)
        | Expr::Same(items)
        | Expr::PathConstructor(items) => {
            for item in items {
                infer_from_expr(item, ctx, params);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                infer_from_expr(arg, ctx, params);
            }
        }
        Expr::Aggregate(agg) => {
            if let Some(e) = &agg.expr {
                infer_from_expr(e, ctx, params);
            }
            if let Some(sep) = &agg.separator {
                infer_from_expr(sep, ctx, params);
            }
        }
        Expr::RecordLiteral(fields) => {
            for (_, v) in fields {
                infer_from_expr(v, ctx, params);
            }
        }
        Expr::LetIn { bindings, body } => {
            for (_, e) in bindings {
                infer_from_expr(e, ctx, params);
            }
            infer_from_expr(body, ctx, params);
        }
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            infer_from_expr(node, ctx, params);
            infer_from_expr(edge, ctx, params);
        }
        Expr::Exists(s) | Expr::ValueSubquery(s) => {
            infer_from_stmt(s, ctx, params);
        }
        Expr::ListIndex { list, index } => {
            infer_from_expr(list, ctx, params);
            infer_from_expr(index, ctx, params);
        }
        Expr::Literal(_) | Expr::Variable(_) | Expr::PathVar(_) | Expr::Parameter { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Inference helpers
// ---------------------------------------------------------------------------

fn try_infer_from_pair(
    maybe_param: &Expr,
    other: &Expr,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    let Expr::Parameter {
        name,
        type_annotation: None,
    } = maybe_param
    else {
        return;
    };
    if let Some(vt) = resolve_expr_type(other, ctx) {
        merge_inferred(params, name, vt);
    }
}

fn try_infer_numeric(
    maybe_param: &Expr,
    other: &Expr,
    ctx: &InferCtx<'_>,
    params: &mut BTreeMap<String, InferredParam>,
) {
    let Expr::Parameter {
        name,
        type_annotation: None,
    } = maybe_param
    else {
        return;
    };
    if let Some(vt) = resolve_expr_type(other, ctx) {
        match vt {
            ValueType::Int8
            | ValueType::Int16
            | ValueType::Int32
            | ValueType::Int64
            | ValueType::Int128
            | ValueType::Int256
            | ValueType::Uint8
            | ValueType::Uint16
            | ValueType::Uint32
            | ValueType::Uint64
            | ValueType::Uint128
            | ValueType::Uint256
            | ValueType::Float32
            | ValueType::Float64 => merge_inferred(params, name, vt),
            _ => {}
        }
    }
}

fn resolve_expr_type(expr: &Expr, ctx: &InferCtx<'_>) -> Option<ValueType> {
    match expr {
        Expr::Literal(v) => literal_value_type(v),
        Expr::PropertyAccess { target, property } => {
            if let Expr::Variable(var) = target.as_ref() {
                ctx.property_type_for(var, property)
            } else {
                None
            }
        }
        Expr::Cast { target_type, .. } => Some(*target_type),
        Expr::FunctionCall { name, .. } => function_return_type(name),
        Expr::UnaryOp { op, expr: inner } => match op {
            UnaryOp::Neg | UnaryOp::Pos => {
                let t = resolve_expr_type(inner, ctx)?;
                (matches!(
                    t,
                    ValueType::Int8
                        | ValueType::Int16
                        | ValueType::Int32
                        | ValueType::Int64
                        | ValueType::Int128
                        | ValueType::Int256
                        | ValueType::Uint8
                        | ValueType::Uint16
                        | ValueType::Uint32
                        | ValueType::Uint64
                        | ValueType::Uint128
                        | ValueType::Uint256
                        | ValueType::Float32
                        | ValueType::Float64
                ))
                .then_some(t)
            }
        },
        // List literal — infer element type from first typed element.
        Expr::ListLiteral(items) => {
            let mut elem_vt: Option<ValueType> = None;
            for item in items {
                if let Some(vt) = resolve_expr_type(item, ctx) {
                    if vt == ValueType::Null {
                        continue;
                    }
                    match elem_vt {
                        None => elem_vt = Some(vt),
                        Some(existing) if existing == vt => {}
                        Some(_) => return Some(ValueType::List), // mixed
                    }
                }
            }
            match elem_vt.and_then(ScalarType::from_value_type) {
                Some(scalar) => Some(ValueType::TypedList(scalar)),
                None => Some(ValueType::List),
            }
        }
        _ => None,
    }
}

fn literal_value_type(v: &gleaph_types::Value) -> Option<ValueType> {
    use gleaph_types::Value;
    match v {
        Value::Int8(_) => Some(ValueType::Int8),
        Value::Int16(_) => Some(ValueType::Int16),
        Value::Int32(_) => Some(ValueType::Int32),
        Value::Int64(_) => Some(ValueType::Int64),
        Value::Int128(_) => Some(ValueType::Int128),
        Value::Int256(_) => Some(ValueType::Int256),
        Value::Uint8(_) => Some(ValueType::Uint8),
        Value::Uint16(_) => Some(ValueType::Uint16),
        Value::Uint32(_) => Some(ValueType::Uint32),
        Value::Uint64(_) => Some(ValueType::Uint64),
        Value::Uint128(_) => Some(ValueType::Uint128),
        Value::Uint256(_) => Some(ValueType::Uint256),
        Value::Float32(_) => Some(ValueType::Float32),
        Value::Float64(_) => Some(ValueType::Float64),
        Value::Text(_) => Some(ValueType::Text),
        Value::Bool(_) => Some(ValueType::Bool),
        Value::Timestamp(_) => Some(ValueType::Timestamp),
        Value::List(items) => Some(infer_list_element_type(items)),
        Value::Null => Some(ValueType::Null),
        Value::Bytes(_) => Some(ValueType::Bytes),
        Value::Date(_) => Some(ValueType::Date),
        Value::Time(_) => Some(ValueType::Time),
        Value::DateTime(_, _) => Some(ValueType::DateTime),
        Value::Duration(_, _) => Some(ValueType::Duration),
        Value::Decimal(_) => Some(ValueType::Decimal),
        Value::Path(_) | Value::Principal(_) => None,
    }
}

/// Infer element type from a list of values.
/// Returns `TypedList(scalar)` when all elements share the same scalar type,
/// otherwise falls back to untyped `List`.
fn infer_list_element_type(items: &[gleaph_types::Value]) -> ValueType {
    if items.is_empty() {
        return ValueType::List;
    }
    let mut elem_type: Option<ValueType> = None;
    for item in items {
        let Some(vt) = literal_value_type(item) else {
            return ValueType::List;
        };
        if vt == ValueType::Null {
            continue; // nulls don't constrain element type
        }
        match elem_type {
            None => elem_type = Some(vt),
            Some(existing) if existing == vt => {}
            Some(_) => return ValueType::List, // mixed types
        }
    }
    match elem_type.and_then(ScalarType::from_value_type) {
        Some(scalar) => ValueType::TypedList(scalar),
        None => ValueType::List,
    }
}

fn function_return_type(name: &str) -> Option<ValueType> {
    match name.to_ascii_lowercase().as_str() {
        "to_string" | "char_length" | "character_length" | "upper" | "lower" | "trim" | "ltrim"
        | "rtrim" | "left" | "right" | "reverse" | "substring" | "replace" => Some(ValueType::Text),
        "size" | "length" | "to_integer" => Some(ValueType::Int64),
        "to_float" => Some(ValueType::Float64),
        "to_boolean" => Some(ValueType::Bool),
        _ => None,
    }
}

fn is_integer_value_type(vt: ValueType) -> bool {
    matches!(
        vt,
        ValueType::Int8
            | ValueType::Int16
            | ValueType::Int32
            | ValueType::Int64
            | ValueType::Int128
            | ValueType::Int256
            | ValueType::Uint8
            | ValueType::Uint16
            | ValueType::Uint32
            | ValueType::Uint64
            | ValueType::Uint128
            | ValueType::Uint256
    )
}

fn is_numeric_op(op: &BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod
    )
}

fn merge_inferred(params: &mut BTreeMap<String, InferredParam>, name: &str, vt: ValueType) {
    let Some(param) = params.get_mut(name) else {
        return;
    };
    if param.explicit {
        if !param.types.is_empty() && !param.types.contains(&vt) && vt != ValueType::Null {
            param.conflicts.push(format!(
                "explicit annotation conflicts with inferred {:?}",
                vt
            ));
        }
        return;
    }

    if param.types.is_empty() {
        param.types = vec![vt];
        return;
    }
    if param.types.contains(&vt) {
        return;
    }
    // Any integer + FLOAT → widen to FLOAT
    if param.types.len() == 1 && is_integer_value_type(param.types[0]) && vt == ValueType::Float64 {
        param.types = vec![ValueType::Float64];
        return;
    }
    if param.types == [ValueType::Float64] && is_integer_value_type(vt) {
        return; // FLOAT covers any integer
    }
    // Same-signedness integer widening: keep the wider type.
    if param.types.len() == 1 && is_integer_value_type(param.types[0]) && is_integer_value_type(vt)
    {
        // Keep the wider variant (or the existing one if same width).
        return;
    }
    // TypedList refines untyped List
    if param.types == [ValueType::List] && matches!(vt, ValueType::TypedList(_)) {
        param.types = vec![vt];
        return;
    }
    if matches!(param.types.as_slice(), [ValueType::TypedList(_)]) && vt == ValueType::List {
        return; // typed list already more specific
    }
    param.conflicts.push(format!(
        "conflicting inferred types: {:?} vs {:?}",
        param.types, vt
    ));
}

// ---------------------------------------------------------------------------
// Conversion to PreparedValueType
// ---------------------------------------------------------------------------

/// Convert an `InferredParam` to the public `PreparedParameterInfo`.
pub fn to_prepared_param_info(
    name: String,
    inferred: &InferredParam,
) -> gleaph_types::PreparedParameterInfo {
    use gleaph_types::PreparedValueType;

    let types: Vec<PreparedValueType> = inferred
        .types
        .iter()
        .map(|vt| match vt {
            ValueType::Int8 => PreparedValueType::Int8,
            ValueType::Int16 => PreparedValueType::Int16,
            ValueType::Int32 => PreparedValueType::Int32,
            ValueType::Int64 => PreparedValueType::Int64,
            ValueType::Int128 => PreparedValueType::Int128,
            ValueType::Int256 => PreparedValueType::Int256,
            ValueType::Uint8 => PreparedValueType::Uint8,
            ValueType::Uint16 => PreparedValueType::Uint16,
            ValueType::Uint32 => PreparedValueType::Uint32,
            ValueType::Uint64 => PreparedValueType::Uint64,
            ValueType::Uint128 => PreparedValueType::Uint128,
            ValueType::Uint256 => PreparedValueType::Uint256,
            ValueType::Float32 => PreparedValueType::Float32,
            ValueType::Float64 => PreparedValueType::Float64,
            ValueType::Text => PreparedValueType::Text,
            ValueType::Bool => PreparedValueType::Bool,
            ValueType::Timestamp => PreparedValueType::Timestamp,
            ValueType::List => PreparedValueType::List,
            ValueType::TypedList(s) => PreparedValueType::TypedList(s.to_prepared()),
            ValueType::Null => PreparedValueType::Null,
            ValueType::Bytes => PreparedValueType::Bytes,
            ValueType::Date => PreparedValueType::Date,
            ValueType::Time => PreparedValueType::Time,
            ValueType::DateTime => PreparedValueType::DateTime,
            ValueType::Duration => PreparedValueType::Duration,
            ValueType::Decimal => PreparedValueType::Decimal,
            ValueType::TextConstrained { .. } => PreparedValueType::Text,
            ValueType::BytesConstrained { .. } => PreparedValueType::Bytes,
        })
        .collect();

    gleaph_types::PreparedParameterInfo {
        name,
        required: inferred.required,
        types,
        inferred: !inferred.explicit,
    }
}

/// Extract conflict diagnostics from inferred parameters.
///
/// Returns a `TypeDiagnostic` for each parameter that has inference conflicts.
pub fn conflict_diagnostics(
    params: &BTreeMap<String, InferredParam>,
) -> Vec<gleaph_types::TypeDiagnostic> {
    params
        .iter()
        .filter(|(_, p)| !p.conflicts.is_empty())
        .flat_map(|(name, p)| {
            p.conflicts
                .iter()
                .map(move |conflict| gleaph_types::TypeDiagnostic {
                    kind: gleaph_types::TypeDiagnosticKind::ParameterInferenceConflict,
                    message: format!("parameter ${name}: {conflict}"),
                })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_statement;
    use crate::semantic::NoSchema;

    fn infer(gql: &str) -> BTreeMap<String, InferredParam> {
        let stmt = parse_statement(gql).expect("parse");
        infer_parameter_types(&stmt, &NoSchema)
    }

    #[test]
    fn explicit_annotation_preserved() {
        let result = infer("MATCH (u:User) WHERE u.age > $min_age :: INT RETURN u");
        let param = result.get("min_age").unwrap();
        assert!(param.explicit);
        assert_eq!(param.types, vec![ValueType::Int32]);
        assert!(param.required);
    }

    #[test]
    fn explicit_nullable_annotation() {
        let result = infer("MATCH (u:User) WHERE u.age > $min_age :: INT | NULL RETURN u");
        let param = result.get("min_age").unwrap();
        assert!(param.explicit);
        assert!(param.types.contains(&ValueType::Int32));
        assert!(param.types.contains(&ValueType::Null));
        assert!(!param.required);
    }

    #[test]
    fn infer_from_literal_comparison() {
        let result = infer("MATCH (u:User) WHERE u.name = $name AND 42 > $min RETURN u");
        let min = result.get("min").unwrap();
        assert!(!min.explicit);
        assert_eq!(min.types, vec![ValueType::Int32]);
    }

    #[test]
    fn infer_from_string_predicate() {
        let result = infer("MATCH (u:User) WHERE u.name STARTS WITH $prefix RETURN u");
        let prefix = result.get("prefix").unwrap();
        assert!(!prefix.explicit);
        assert_eq!(prefix.types, vec![ValueType::Text]);
    }

    #[test]
    fn infer_from_concat() {
        let result = infer("MATCH (u:User) RETURN u.name || $suffix AS full_name");
        let suffix = result.get("suffix").unwrap();
        assert!(!suffix.explicit);
        assert_eq!(suffix.types, vec![ValueType::Text]);
    }

    #[test]
    fn numeric_widening() {
        let result = infer("MATCH (u:User) WHERE u.score > 1.5 + $delta RETURN u");
        let delta = result.get("delta").unwrap();
        assert_eq!(delta.types, vec![ValueType::Float64]);
    }

    #[test]
    fn conflict_clears_types() {
        let result = infer("MATCH (u:User) WHERE 42 = $x AND 'hello' = $x RETURN u");
        let x = result.get("x").unwrap();
        assert!(x.types.is_empty(), "conflict should clear types");
        assert!(!x.conflicts.is_empty());
    }

    #[test]
    fn no_override_explicit_with_inferred() {
        let result = infer("MATCH (u:User) WHERE $x :: INT > 1.5 RETURN u");
        let x = result.get("x").unwrap();
        assert!(x.explicit);
        assert_eq!(x.types, vec![ValueType::Int32]);
    }

    #[test]
    fn unknown_when_no_context() {
        let result = infer("MATCH (u:User) WHERE u.age > $min_age RETURN u");
        let param = result.get("min_age").unwrap();
        assert!(param.types.is_empty());
    }

    #[test]
    fn to_prepared_param_info_conversion() {
        let ip = InferredParam {
            types: vec![ValueType::Int64],
            explicit: false,
            required: true,
            conflicts: vec![],
        };
        let ppi = to_prepared_param_info("age".into(), &ip);
        assert_eq!(ppi.name, "age");
        assert!(ppi.required);
        assert!(ppi.inferred);
        assert_eq!(ppi.types, vec![gleaph_types::PreparedValueType::Int64]);
    }

    // Schema-based tests
    struct TestSchema;
    impl crate::semantic::PropertySchema for TestSchema {
        fn node_property_types(&self, labels: &[String]) -> Vec<(String, ValueType, bool)> {
            if labels.contains(&"User".to_string()) {
                vec![
                    ("name".into(), ValueType::Text, true),
                    ("age".into(), ValueType::Int64, false),
                    ("score".into(), ValueType::Float64, false),
                ]
            } else {
                vec![]
            }
        }
        fn edge_property_types(&self, label: &str) -> Vec<(String, ValueType, bool)> {
            if label == "FOLLOWS" {
                vec![("since".into(), ValueType::Timestamp, false)]
            } else {
                vec![]
            }
        }
    }

    #[test]
    fn schema_infer_from_property_comparison() {
        let stmt =
            parse_statement("MATCH (u:User) WHERE u.age >= $min_age AND u.name = $name RETURN u")
                .unwrap();
        let result = infer_parameter_types(&stmt, &TestSchema);

        let min_age = result.get("min_age").unwrap();
        assert_eq!(min_age.types, vec![ValueType::Int64]);
        assert!(!min_age.explicit);

        let name = result.get("name").unwrap();
        assert_eq!(name.types, vec![ValueType::Text]);
        assert!(!name.explicit);
    }

    #[test]
    fn schema_infer_float_from_property() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.score > $threshold RETURN u").unwrap();
        let result = infer_parameter_types(&stmt, &TestSchema);

        let threshold = result.get("threshold").unwrap();
        assert_eq!(threshold.types, vec![ValueType::Float64]);
    }

    #[test]
    fn conflict_diagnostics_produced() {
        let result = infer("MATCH (u:User) WHERE 42 = $x AND 'hello' = $x RETURN u");
        let diags = conflict_diagnostics(&result);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].kind,
            gleaph_types::TypeDiagnosticKind::ParameterInferenceConflict
        );
        assert!(diags[0].message.contains("$x"));
    }

    #[test]
    fn no_conflict_diagnostics_for_clean_inference() {
        let result = infer("MATCH (u:User) WHERE 42 > $min RETURN u");
        let diags = conflict_diagnostics(&result);
        assert!(diags.is_empty());
    }

    #[test]
    fn multiple_conflict_diagnostics() {
        // Two different conflicts on the same parameter produce multiple diagnostics.
        // (This test uses the conflict scenario: INT vs TEXT on an inferred param.)
        let result = infer("MATCH (u:User) WHERE 42 = $x AND 'hello' = $x RETURN u");
        let diags = conflict_diagnostics(&result);
        assert!(!diags.is_empty());
        for d in &diags {
            assert_eq!(
                d.kind,
                gleaph_types::TypeDiagnosticKind::ParameterInferenceConflict
            );
            assert!(d.message.contains("$x"));
        }
    }

    #[test]
    fn schema_infer_from_edge_property() {
        let stmt = parse_statement(
            "MATCH (a:User)-[f:FOLLOWS]->(b:User) WHERE f.since > $after RETURN a, b",
        )
        .unwrap();
        let result = infer_parameter_types(&stmt, &TestSchema);

        let after = result.get("after").unwrap();
        assert_eq!(after.types, vec![ValueType::Timestamp]);
    }

    // --- Typed list inference tests ---

    #[test]
    fn explicit_typed_list_annotation() {
        let result = infer("MATCH (u:User) WHERE u.tags = $tags :: LIST<TEXT> RETURN u");
        let tags = result.get("tags").unwrap();
        assert!(tags.explicit);
        assert_eq!(tags.types, vec![ValueType::TypedList(ScalarType::Text)]);
    }

    #[test]
    fn explicit_typed_list_int_annotation() {
        let result = infer("MATCH (u:User) WHERE u.ids = $ids :: LIST<INT> RETURN u");
        let ids = result.get("ids").unwrap();
        assert!(ids.explicit);
        assert_eq!(ids.types, vec![ValueType::TypedList(ScalarType::Int32)]);
    }

    #[test]
    fn typed_list_union_annotation() {
        let result = infer("MATCH (u:User) WHERE u.data = $d :: LIST<INT> | LIST<TEXT> RETURN u");
        let d = result.get("d").unwrap();
        assert!(d.explicit);
        assert_eq!(
            d.types,
            vec![
                ValueType::TypedList(ScalarType::Int32),
                ValueType::TypedList(ScalarType::Text),
            ]
        );
    }

    #[test]
    fn infer_typed_list_from_literal_comparison() {
        let result = infer("MATCH (u:User) WHERE $tags = [1, 2, 3] RETURN u");
        let tags = result.get("tags").unwrap();
        assert!(!tags.explicit);
        assert_eq!(tags.types, vec![ValueType::TypedList(ScalarType::Int32)]);
    }

    #[test]
    fn infer_untyped_list_from_mixed_literal() {
        let result = infer("MATCH (u:User) WHERE $data = [1, 'hello'] RETURN u");
        let data = result.get("data").unwrap();
        assert_eq!(data.types, vec![ValueType::List]);
    }

    #[test]
    fn typed_list_refines_untyped() {
        // First encounter says List (untyped), second gives TypedList(Int).
        // TypedList should win.
        let result = infer("MATCH (u:User) WHERE $x = [] AND $x = [1, 2] RETURN u");
        let x = result.get("x").unwrap();
        assert_eq!(x.types, vec![ValueType::TypedList(ScalarType::Int32)]);
    }

    #[test]
    fn typed_list_conflict() {
        let result = infer("MATCH (u:User) WHERE [1, 2] = $x AND ['a', 'b'] = $x RETURN u");
        let x = result.get("x").unwrap();
        assert!(x.types.is_empty(), "conflict should clear types");
        assert!(!x.conflicts.is_empty());
    }

    #[test]
    fn to_prepared_param_typed_list() {
        let ip = InferredParam {
            types: vec![ValueType::TypedList(ScalarType::Int64)],
            explicit: true,
            required: true,
            conflicts: vec![],
        };
        let ppi = to_prepared_param_info("ids".into(), &ip);
        assert_eq!(
            ppi.types,
            vec![gleaph_types::PreparedValueType::TypedList(
                gleaph_types::PreparedScalarType::Int64
            )]
        );
    }

    // --- IN $param inference tests ---

    #[test]
    fn in_param_parses() {
        // Verify `x IN $param` actually parses.
        let stmt = parse_statement("MATCH (u:User) WHERE u.age IN $ages RETURN u");
        assert!(stmt.is_ok(), "IN $param should parse: {:?}", stmt.err());
    }

    #[test]
    fn in_param_infer_typed_list_from_schema() {
        // u.age is INT in TestSchema → $ages should be LIST<INT>.
        let stmt = parse_statement("MATCH (u:User) WHERE u.age IN $ages RETURN u").unwrap();
        let result = infer_parameter_types(&stmt, &TestSchema);
        let ages = result.get("ages").unwrap();
        assert_eq!(
            ages.types,
            vec![ValueType::TypedList(ScalarType::Int64)],
            "IN $param should infer TypedList from schema"
        );
    }

    #[test]
    fn in_param_infer_typed_list_text_from_schema() {
        // u.name is TEXT in TestSchema → $names should be LIST<TEXT>.
        let stmt = parse_statement("MATCH (u:User) WHERE u.name IN $names RETURN u").unwrap();
        let result = infer_parameter_types(&stmt, &TestSchema);
        let names = result.get("names").unwrap();
        assert_eq!(names.types, vec![ValueType::TypedList(ScalarType::Text)],);
    }

    #[test]
    fn in_literal_list_still_infers_element_type() {
        // Existing behavior: `u.age IN [$min, $max]` → each param is INT.
        let stmt = parse_statement("MATCH (u:User) WHERE u.age IN [$min, $max] RETURN u").unwrap();
        let result = infer_parameter_types(&stmt, &TestSchema);
        let min = result.get("min").unwrap();
        assert_eq!(min.types, vec![ValueType::Int64]);
        let max = result.get("max").unwrap();
        assert_eq!(max.types, vec![ValueType::Int64]);
    }

    #[test]
    fn in_param_with_literal_comparison() {
        // 42 IN $param → infer LIST<INT> from the literal type.
        let result = infer("MATCH (u:User) WHERE 42 IN $ids RETURN u");
        let ids = result.get("ids").unwrap();
        assert_eq!(ids.types, vec![ValueType::TypedList(ScalarType::Int32)]);
    }
}
