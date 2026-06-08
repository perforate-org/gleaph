//! Property projection for partial record hydration at scan, expand, and edge-bind operators.

use std::collections::BTreeSet;
use std::rc::Rc;

use gleaph_gql::ast::{Expr, ExprKind, LetBinding};

use crate::expr_children::for_each_immediate_child_expr;
use crate::plan::{AggregateSpec, PlanOp, SetPlanItem, Str};

/// Walks the plan tree and fills projection fields on scan/expand/bind operators when safe.
pub fn apply_node_property_projections(ops: &mut [PlanOp]) {
    apply_recursive(ops, &[]);
}

#[derive(Clone)]
enum ScanProjectionPatch {
    Unchanged,
    FullProperties,
    Projected(Rc<[Str]>),
}

#[derive(Clone)]
struct OpProjectionPatch {
    vertex_scan: ScanProjectionPatch,
    edge_scan: ScanProjectionPatch,
    expand_edge: ScanProjectionPatch,
    expand_dst: ScanProjectionPatch,
    bind_near: ScanProjectionPatch,
    bind_far: ScanProjectionPatch,
}

impl OpProjectionPatch {
    fn noop() -> Self {
        Self {
            vertex_scan: ScanProjectionPatch::Unchanged,
            edge_scan: ScanProjectionPatch::Unchanged,
            expand_edge: ScanProjectionPatch::Unchanged,
            expand_dst: ScanProjectionPatch::Unchanged,
            bind_near: ScanProjectionPatch::Unchanged,
            bind_far: ScanProjectionPatch::Unchanged,
        }
    }

    /// All scan/expand/bind slots should use empty projected maps.
    fn all_empty() -> Self {
        Self {
            vertex_scan: ScanProjectionPatch::Projected(Rc::from(
                Vec::<Str>::new().into_boxed_slice(),
            )),
            edge_scan: ScanProjectionPatch::Projected(Rc::from(
                Vec::<Str>::new().into_boxed_slice(),
            )),
            expand_edge: ScanProjectionPatch::Projected(Rc::from(
                Vec::<Str>::new().into_boxed_slice(),
            )),
            expand_dst: ScanProjectionPatch::Projected(Rc::from(
                Vec::<Str>::new().into_boxed_slice(),
            )),
            bind_near: ScanProjectionPatch::Projected(Rc::from(
                Vec::<Str>::new().into_boxed_slice(),
            )),
            bind_far: ScanProjectionPatch::Projected(Rc::from(
                Vec::<Str>::new().into_boxed_slice(),
            )),
        }
    }
}

fn apply_recursive(ops: &mut [PlanOp], tail: &[&[PlanOp]]) {
    let n = ops.len();
    let mut patches: Vec<OpProjectionPatch> = Vec::with_capacity(n);
    for i in 0..n {
        let mut expr_refs = Vec::new();
        for op in &ops[i + 1..] {
            collect_exprs_from_op_deep(op, &mut expr_refs);
        }
        for seg in tail {
            for op in *seg {
                collect_exprs_from_op_deep(op, &mut expr_refs);
            }
        }
        let patch = if projection_inference_may_be_needed(&ops[i], &expr_refs) {
            op_projection_patch(&ops[i], &expr_refs)
        } else {
            OpProjectionPatch::all_empty()
        };
        patches.push(patch);
    }
    for i in 0..n {
        apply_op_projection_patch(&mut ops[i], &patches[i]);
    }

    for i in 0..n {
        let (before, after) = ops.split_at_mut(i + 1);
        let op = &mut before[i];
        let parent_rest: &[PlanOp] = after;
        match op {
            PlanOp::HashJoin { left, right, .. } => {
                apply_recursive(left, &[right.as_slice(), parent_rest]);
                apply_recursive(right, &[parent_rest]);
            }
            PlanOp::CartesianProduct { left, right } => {
                apply_recursive(left, &[right.as_slice(), parent_rest]);
                apply_recursive(right, &[parent_rest]);
            }
            PlanOp::OptionalMatch { sub_plan } => {
                apply_recursive(sub_plan, &[parent_rest]);
            }
            PlanOp::InlineProcedureCall { sub_plan, .. } => {
                apply_recursive(&mut sub_plan.ops, &[]);
            }
            PlanOp::SetOperation { right, .. } => {
                apply_recursive(&mut right.ops, &[]);
            }
            PlanOp::UseGraph {
                sub_plan: Some(sp), ..
            } => {
                apply_recursive(sp, &[]);
            }
            _ => {}
        }
    }
}

fn infer_vertex_scan_patch(exprs: &[&Expr], var: &str) -> ScanProjectionPatch {
    match infer_projection_names(exprs, var) {
        None => ScanProjectionPatch::FullProperties,
        Some(set) => ScanProjectionPatch::Projected(names_to_rc(set)),
    }
}

fn indexed_entity_scan_patch(
    exprs: &[&Expr],
    var: &str,
    index_property: &str,
) -> ScanProjectionPatch {
    match infer_projection_names(exprs, var) {
        None => ScanProjectionPatch::FullProperties,
        Some(mut set) => {
            set.insert(index_property.to_string());
            ScanProjectionPatch::Projected(names_to_rc(set))
        }
    }
}

fn merge_index_property_into_patch(
    patch: ScanProjectionPatch,
    index_property: &str,
) -> ScanProjectionPatch {
    match patch {
        ScanProjectionPatch::Unchanged | ScanProjectionPatch::FullProperties => patch,
        ScanProjectionPatch::Projected(rc) => {
            let mut set: BTreeSet<String> = rc.iter().map(|s| s.to_string()).collect();
            set.insert(index_property.to_string());
            ScanProjectionPatch::Projected(names_to_rc(set))
        }
    }
}

fn expand_edge_patch(
    exprs: &[&Expr],
    edge_var: &str,
    indexed_property: Option<&str>,
) -> ScanProjectionPatch {
    let mut patch = infer_vertex_scan_patch(exprs, edge_var);
    if let Some(prop) = indexed_property {
        patch = merge_index_property_into_patch(patch, prop);
    }
    patch
}

fn op_projection_patch(op: &PlanOp, exprs: &[&Expr]) -> OpProjectionPatch {
    let mut p = OpProjectionPatch::noop();
    match op {
        PlanOp::NodeScan { variable, .. } => {
            p.vertex_scan = infer_vertex_scan_patch(exprs, variable.as_ref());
        }
        PlanOp::IndexScan {
            variable, property, ..
        } => {
            p.vertex_scan = indexed_entity_scan_patch(exprs, variable.as_ref(), property.as_ref());
        }
        PlanOp::IndexIntersection {
            variable, scans, ..
        } => match infer_projection_names(exprs, variable.as_ref()) {
            None => p.vertex_scan = ScanProjectionPatch::FullProperties,
            Some(mut set) => {
                for s in scans {
                    set.insert(s.property.to_string());
                }
                p.vertex_scan = ScanProjectionPatch::Projected(names_to_rc(set));
            }
        },
        PlanOp::ConditionalIndexScan {
            fallback_variable, ..
        } => {
            p.vertex_scan = infer_vertex_scan_patch(exprs, fallback_variable.as_ref());
        }
        PlanOp::EdgeIndexScan {
            variable, property, ..
        } => {
            p.edge_scan = indexed_entity_scan_patch(exprs, variable.as_ref(), property.as_ref());
        }
        PlanOp::Expand {
            edge,
            dst,
            indexed_edge_equality,
            ..
        } => {
            let idx_prop = indexed_edge_equality
                .as_ref()
                .map(|(prop, _)| prop.as_ref());
            p.expand_edge = expand_edge_patch(exprs, edge.as_ref(), idx_prop);
            p.expand_dst = infer_vertex_scan_patch(exprs, dst.as_ref());
        }
        PlanOp::ExpandFilter {
            edge,
            dst,
            indexed_edge_equality,
            dst_filter,
            ..
        } => {
            let idx_prop = indexed_edge_equality
                .as_ref()
                .map(|(prop, _)| prop.as_ref());
            p.expand_edge = expand_edge_patch(exprs, edge.as_ref(), idx_prop);
            let mut dst_exprs: Vec<&Expr> = dst_filter.iter().collect();
            dst_exprs.extend(exprs.iter().copied());
            p.expand_dst = infer_vertex_scan_patch(&dst_exprs, dst.as_ref());
        }
        PlanOp::EdgeBindEndpoints { near, far, .. } => {
            p.bind_near = infer_vertex_scan_patch(exprs, near.as_ref());
            p.bind_far = infer_vertex_scan_patch(exprs, far.as_ref());
        }
        _ => {}
    }
    p
}

fn apply_projection_slot(slot: &mut Option<Rc<[Str]>>, patch: &ScanProjectionPatch) {
    match patch {
        ScanProjectionPatch::Unchanged => {}
        ScanProjectionPatch::FullProperties => *slot = None,
        ScanProjectionPatch::Projected(rc) => *slot = Some(rc.clone()),
    }
}

fn apply_op_projection_patch(op: &mut PlanOp, patch: &OpProjectionPatch) {
    match &patch.vertex_scan {
        ScanProjectionPatch::Unchanged => {}
        ScanProjectionPatch::FullProperties => {
            if let PlanOp::NodeScan {
                property_projection,
                ..
            }
            | PlanOp::IndexScan {
                property_projection,
                ..
            }
            | PlanOp::IndexIntersection {
                property_projection,
                ..
            }
            | PlanOp::ConditionalIndexScan {
                property_projection,
                ..
            } = op
            {
                *property_projection = None;
            }
        }
        ScanProjectionPatch::Projected(rc) => {
            if let PlanOp::NodeScan {
                property_projection,
                ..
            }
            | PlanOp::IndexScan {
                property_projection,
                ..
            }
            | PlanOp::IndexIntersection {
                property_projection,
                ..
            }
            | PlanOp::ConditionalIndexScan {
                property_projection,
                ..
            } = op
            {
                *property_projection = Some(rc.clone());
            }
        }
    }

    if let PlanOp::EdgeIndexScan {
        property_projection,
        ..
    } = op
    {
        apply_projection_slot(property_projection, &patch.edge_scan);
    }

    match op {
        PlanOp::Expand {
            edge_property_projection,
            dst_property_projection,
            ..
        }
        | PlanOp::ExpandFilter {
            edge_property_projection,
            dst_property_projection,
            ..
        } => {
            apply_projection_slot(edge_property_projection, &patch.expand_edge);
            apply_projection_slot(dst_property_projection, &patch.expand_dst);
        }
        PlanOp::EdgeBindEndpoints {
            near_property_projection,
            far_property_projection,
            ..
        } => {
            apply_projection_slot(near_property_projection, &patch.bind_near);
            apply_projection_slot(far_property_projection, &patch.bind_far);
        }
        _ => {}
    }
}

fn infer_projection_names(exprs: &[&Expr], var: &str) -> Option<BTreeSet<String>> {
    for e in exprs {
        if var_used_as_non_property_receiver(e, var) {
            return None;
        }
    }
    let mut out = BTreeSet::new();
    for e in exprs {
        collect_property_names_on_var(e, var, &mut out);
    }
    Some(out)
}

fn names_to_rc(names: BTreeSet<String>) -> Rc<[Str]> {
    let v: Vec<Str> = names.into_iter().map(Str::from).collect();
    Rc::from(v.into_boxed_slice())
}

fn var_used_as_non_property_receiver(expr: &Expr, var: &str) -> bool {
    match &expr.kind {
        ExprKind::PropertyAccess { expr: base, .. } => {
            if matches!(&base.kind, ExprKind::Variable(v) if v == var) {
                false
            } else {
                var_used_as_non_property_receiver(base, var)
            }
        }
        ExprKind::Variable(v) => v == var,
        _ => {
            let mut any = false;
            for_each_immediate_child_expr(expr, |c| {
                any |= var_used_as_non_property_receiver(c, var);
            });
            any
        }
    }
}

fn chain_root_is_var(expr: &Expr, var: &str) -> bool {
    match &expr.kind {
        ExprKind::Variable(v) => v == var,
        ExprKind::PropertyAccess { expr: base, .. } => chain_root_is_var(base, var),
        _ => false,
    }
}

fn collect_property_names_on_var(expr: &Expr, var: &str, out: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::PropertyAccess {
            expr: base,
            property,
        } => {
            if chain_root_is_var(base, var) {
                out.insert(property.clone());
            }
            collect_property_names_on_var(base, var, out);
        }
        _ => {
            for_each_immediate_child_expr(expr, |c| {
                collect_property_names_on_var(c, var, out);
            });
        }
    }
}

/// True when downstream expressions or this op may reference record properties, so inference
/// should run. Otherwise we skip per-variable walks and apply full-property hydration.
fn projection_inference_may_be_needed(op: &PlanOp, suffix_exprs: &[&Expr]) -> bool {
    if !suffix_exprs.is_empty() {
        return true;
    }
    if expr_list_contains_property_access(suffix_exprs) {
        return true;
    }
    match op {
        PlanOp::Expand {
            indexed_edge_equality,
            ..
        } => indexed_edge_equality.is_some(),
        PlanOp::ExpandFilter {
            indexed_edge_equality,
            dst_filter,
            ..
        } => {
            indexed_edge_equality.is_some() || dst_filter.iter().any(expr_contains_property_access)
        }
        _ => false,
    }
}

fn expr_list_contains_property_access(exprs: &[&Expr]) -> bool {
    exprs.iter().any(|e| expr_contains_property_access(e))
}

fn expr_contains_property_access(expr: &Expr) -> bool {
    if matches!(expr.kind, ExprKind::PropertyAccess { .. }) {
        return true;
    }
    let mut any = false;
    for_each_immediate_child_expr(expr, |c| {
        any |= expr_contains_property_access(c);
    });
    any
}

fn collect_exprs_from_op_deep<'a>(op: &'a PlanOp, out: &mut Vec<&'a Expr>) {
    match op {
        PlanOp::PropertyFilter { predicates, .. } => {
            for p in predicates {
                out.push(p);
            }
        }
        PlanOp::ExpandFilter { dst_filter, .. } => {
            for p in dst_filter {
                out.push(p);
            }
        }
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
            for c in columns {
                out.push(&c.expr);
            }
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for e in group_by {
                out.push(e);
            }
            for a in aggregates {
                push_aggregate_exprs(a, out);
            }
        }
        PlanOp::Sort { order_by } => {
            for item in &order_by.items {
                out.push(&item.expr);
            }
        }
        PlanOp::Limit { count, offset } => {
            if let Some(e) = count {
                out.push(e);
            }
            if let Some(e) = offset {
                out.push(e);
            }
        }
        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => {
            for item in &order_by.items {
                out.push(&item.expr);
            }
            out.push(k);
            if let Some(e) = offset {
                out.push(e);
            }
        }
        PlanOp::Let { bindings } => {
            for b in bindings {
                push_let_binding_exprs(b, out);
            }
        }
        PlanOp::For { list, .. } => {
            out.push(list);
        }
        PlanOp::Filter { condition } => {
            out.push(condition);
        }
        PlanOp::CallProcedure { args, .. } => {
            for a in args {
                out.push(a);
            }
        }
        PlanOp::InsertVertex { properties, .. } => {
            for p in properties {
                out.push(&p.value);
            }
        }
        PlanOp::InsertEdge { properties, .. } => {
            for p in properties {
                out.push(&p.value);
            }
        }
        PlanOp::SetProperties { items } => {
            for item in items {
                if let SetPlanItem::Property { value, .. }
                | SetPlanItem::AllProperties { value, .. } = item
                {
                    out.push(value);
                }
            }
        }
        PlanOp::RemoveProperties { .. } => {}
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            collect_exprs_from_ops_slice(&sub_plan.ops, out);
        }
        PlanOp::HashJoin { left, right, .. } => {
            collect_exprs_from_ops_slice(left, out);
            collect_exprs_from_ops_slice(right, out);
        }
        PlanOp::CartesianProduct { left, right } => {
            collect_exprs_from_ops_slice(left, out);
            collect_exprs_from_ops_slice(right, out);
        }
        PlanOp::OptionalMatch { sub_plan } => {
            collect_exprs_from_ops_slice(sub_plan, out);
        }
        PlanOp::UseGraph {
            sub_plan: Some(sp), ..
        } => {
            collect_exprs_from_ops_slice(sp, out);
        }
        PlanOp::SetOperation { right, .. } => {
            collect_exprs_from_ops_slice(&right.ops, out);
        }
        _ => {}
    }
}

fn push_aggregate_exprs<'a>(a: &'a AggregateSpec, out: &mut Vec<&'a Expr>) {
    if let Some(e) = &a.expr {
        out.push(e);
    }
    if let Some(e) = &a.expr2 {
        out.push(e);
    }
    if let Some(e) = &a.filter {
        out.push(e);
    }
    if let Some(ob) = &a.order_by {
        for item in &ob.items {
            out.push(&item.expr);
        }
    }
}

fn push_let_binding_exprs<'a>(b: &'a LetBinding, out: &mut Vec<&'a Expr>) {
    out.push(&b.value);
}

fn collect_exprs_from_ops_slice<'a>(ops: &'a [PlanOp], out: &mut Vec<&'a Expr>) {
    for op in ops {
        collect_exprs_from_op_deep(op, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::Expr;
    use gleaph_gql::types::EdgeDirection;

    #[test]
    fn projection_includes_property_access_only() {
        let q = Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::new(ExprKind::Variable("n".into()))),
            property: "score".into(),
        });
        let names = infer_projection_names(&[&q], "n").expect("subset");
        assert!(names.contains("score"));
    }

    #[test]
    fn whole_node_disables_projection() {
        let q = Expr::new(ExprKind::Variable("n".into()));
        assert!(infer_projection_names(&[&q], "n").is_none());
    }

    #[test]
    fn empty_collected_property_set_still_projects_empty_map() {
        assert!(matches!(
            infer_vertex_scan_patch(&[], "n"),
            ScanProjectionPatch::Projected(rc) if rc.is_empty()
        ));
    }

    #[test]
    fn no_downstream_expressions_project_empty_expand_maps() {
        let mut ops = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Person".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_payload_predicate: None,
                edge_vector_predicate: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
                near_group_var: None,
                far_group_var: None,
                path_var: None,
                emit_path_binding: false,
            },
        ];
        apply_node_property_projections(&mut ops);
        let PlanOp::Expand {
            edge_property_projection,
            dst_property_projection,
            ..
        } = &ops[1]
        else {
            panic!("expected Expand");
        };
        assert!(matches!(edge_property_projection, Some(rc) if rc.is_empty()));
        assert!(matches!(dst_property_projection, Some(rc) if rc.is_empty()));
    }
}
