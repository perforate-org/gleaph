use rapidhash::RapidHashSet;

use crate::plan::{PlanAnnotations, PlanOp};

use super::vars::collect_variables_ref;

/// Prune unnecessary `ShortestPath` output bindings when downstream ops don't read them.
pub fn apply_shortest_path_binding_pruning(ops: &mut [PlanOp], _annotations: &mut PlanAnnotations) {
    let mut live = LiveBindings::default();
    prune_shortest_path_bindings_in_ops(ops, &mut live);
}

#[derive(Clone, Default)]
struct LiveBindings {
    all: bool,
    vars: RapidHashSet<String>,
}

impl LiveBindings {
    fn all() -> Self {
        Self {
            all: true,
            vars: RapidHashSet::default(),
        }
    }

    fn contains(&self, var: &str) -> bool {
        self.all || self.vars.contains(var)
    }

    fn insert(&mut self, var: impl Into<String>) {
        if !self.all {
            self.vars.insert(var.into());
        }
    }

    fn remove(&mut self, var: &str) {
        if !self.all {
            self.vars.remove(var);
        }
    }

    fn union(&mut self, other: Self) {
        if self.all || other.all {
            *self = Self::all();
        } else {
            self.vars.extend(other.vars);
        }
    }
}

fn prune_shortest_path_bindings_in_ops(ops: &mut [PlanOp], live: &mut LiveBindings) {
    for op in ops.iter_mut().rev() {
        match op {
            PlanOp::UseGraph {
                sub_plan: Some(sub_plan),
                ..
            } => prune_shortest_path_bindings_in_ops(sub_plan, live),
            PlanOp::OptionalMatch { sub_plan } => {
                let after_optional = live.clone();
                let mut sub_live = live.clone();
                prune_shortest_path_bindings_in_ops(sub_plan, &mut sub_live);
                *live = after_optional;
                live.union(sub_live);
            }
            PlanOp::HashJoin {
                left,
                right,
                join_keys,
            } => {
                let after_join = live.clone();
                let mut left_live = after_join.clone();
                let mut right_live = after_join.clone();
                for key in join_keys {
                    left_live.insert(key.to_string());
                    right_live.insert(key.to_string());
                }
                prune_shortest_path_bindings_in_ops(left, &mut left_live);
                prune_shortest_path_bindings_in_ops(right, &mut right_live);
                *live = after_join;
                live.union(left_live);
                live.union(right_live);
            }
            PlanOp::CartesianProduct { left, right } => {
                let after_product = live.clone();
                let mut left_live = after_product.clone();
                let mut right_live = after_product.clone();
                prune_shortest_path_bindings_in_ops(left, &mut left_live);
                prune_shortest_path_bindings_in_ops(right, &mut right_live);
                *live = after_product;
                live.union(left_live);
                live.union(right_live);
            }
            PlanOp::SetOperation { right, .. } => {
                let after_set = live.clone();
                let mut right_live = live.clone();
                prune_shortest_path_bindings_in_ops(&mut right.ops, &mut right_live);
                *live = after_set;
                live.union(right_live);
            }
            PlanOp::InlineProcedureCall { sub_plan, .. } => {
                let after_call = live.clone();
                let mut sub_live = live.clone();
                prune_shortest_path_bindings_in_ops(&mut sub_plan.ops, &mut sub_live);
                *live = after_call;
                live.union(sub_live);
            }
            PlanOp::ShortestPath {
                edge,
                path_var,
                emit_edge_binding,
                emit_path_binding,
                ..
            } => {
                *emit_edge_binding = live.contains(edge.as_ref());
                *emit_path_binding = path_var
                    .as_ref()
                    .is_some_and(|path_var| live.contains(path_var.as_ref()));
            }
            PlanOp::ExpandFilter {
                edge,
                dst_filter,
                emit_edge_binding,
                var_len,
                path_var,
                emit_path_binding,
                ..
            } => {
                *emit_edge_binding =
                    live.contains(edge.as_ref()) || exprs_reference_var(dst_filter, edge.as_ref());
                if var_len.is_some() {
                    *emit_path_binding = path_var
                        .as_ref()
                        .is_some_and(|path_var| live.contains(path_var.as_ref()));
                }
            }
            PlanOp::Expand {
                edge,
                emit_edge_binding,
                var_len,
                path_var,
                emit_path_binding,
                ..
            } => {
                *emit_edge_binding = live.contains(edge.as_ref());
                if var_len.is_some() {
                    *emit_path_binding = path_var
                        .as_ref()
                        .is_some_and(|path_var| live.contains(path_var.as_ref()));
                }
            }
            _ => {}
        }
        update_live_before_op(op, live);
    }
}

fn update_live_before_op(op: &PlanOp, live: &mut LiveBindings) {
    match op {
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
            if columns.is_empty() {
                *live = LiveBindings::all();
            } else {
                live.all = false;
                live.vars.clear();
                for col in columns {
                    add_expr_vars_to_live(&col.expr, live);
                }
            }
        }
        PlanOp::PropertyFilter { predicates, .. } => {
            for pred in predicates {
                add_expr_vars_to_live(pred, live);
            }
        }
        PlanOp::Filter { condition } => add_expr_vars_to_live(condition, live),
        PlanOp::Sort { order_by } | PlanOp::TopK { order_by, .. } => {
            for item in &order_by.items {
                add_expr_vars_to_live(&item.expr, live);
            }
        }
        PlanOp::Limit { count, offset } => {
            if let Some(count) = count {
                add_expr_vars_to_live(count, live);
            }
            if let Some(offset) = offset {
                add_expr_vars_to_live(offset, live);
            }
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for expr in group_by {
                add_expr_vars_to_live(expr, live);
            }
            for agg in aggregates {
                if let Some(expr) = &agg.expr {
                    add_expr_vars_to_live(expr, live);
                }
                if let Some(expr2) = &agg.expr2 {
                    add_expr_vars_to_live(expr2, live);
                }
                if let Some(filter) = &agg.filter {
                    add_expr_vars_to_live(filter, live);
                }
                if let Some(order_by) = &agg.order_by {
                    for item in &order_by.items {
                        add_expr_vars_to_live(&item.expr, live);
                    }
                }
            }
        }
        PlanOp::ShortestPath {
            src,
            dst,
            edge,
            path_var,
            ..
        } => {
            live.remove(edge.as_ref());
            if let Some(path_var) = path_var {
                live.remove(path_var.as_ref());
            }
            live.insert(src.to_string());
            live.insert(dst.to_string());
        }
        PlanOp::Expand {
            src,
            edge,
            dst,
            hop_aux_binding,
            ..
        } => {
            live.remove(edge.as_ref());
            live.remove(dst.as_ref());
            if let Some(hop_aux_binding) = hop_aux_binding {
                live.remove(hop_aux_binding.as_ref());
            }
            live.insert(src.to_string());
        }
        PlanOp::ExpandFilter {
            src,
            edge,
            dst,
            dst_filter,
            hop_aux_binding,
            ..
        } => {
            for pred in dst_filter {
                add_expr_vars_to_live(pred, live);
            }
            live.remove(edge.as_ref());
            live.remove(dst.as_ref());
            if let Some(hop_aux_binding) = hop_aux_binding {
                live.remove(hop_aux_binding.as_ref());
            }
            live.insert(src.to_string());
        }
        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            hop_aux_binding,
            ..
        } => {
            live.remove(edge.as_ref());
            live.remove(near.as_ref());
            live.remove(far.as_ref());
            if let Some(hop_aux_binding) = hop_aux_binding {
                live.remove(hop_aux_binding.as_ref());
            }
        }
        PlanOp::NodeScan { variable, .. }
        | PlanOp::IndexScan { variable, .. }
        | PlanOp::EdgeIndexScan { variable, .. }
        | PlanOp::ConditionalIndexScan {
            fallback_variable: variable,
            ..
        }
        | PlanOp::IndexIntersection { variable, .. } => {
            live.remove(variable.as_ref());
        }
        PlanOp::Let { bindings } => {
            for binding in bindings {
                live.remove(binding.variable.as_str());
                add_expr_vars_to_live(&binding.value, live);
            }
        }
        PlanOp::For {
            variable,
            list,
            ordinality,
            ..
        } => {
            live.remove(variable.as_ref());
            if let Some(ordinality) = ordinality {
                live.remove(ordinality.as_ref());
            }
            add_expr_vars_to_live(list, live);
        }
        PlanOp::InsertVertex {
            variable: Some(variable),
            ..
        }
        | PlanOp::InsertEdge {
            variable: Some(variable),
            ..
        } => {
            live.remove(variable.as_ref());
        }
        PlanOp::InsertEdge { src, dst, .. } => {
            live.insert(src.to_string());
            live.insert(dst.to_string());
        }
        PlanOp::CallProcedure {
            yield_columns: Some(columns),
            args,
            ..
        } => {
            for col in columns {
                live.remove(col.alias.as_ref().unwrap_or(&col.name).as_ref());
            }
            for arg in args {
                add_expr_vars_to_live(arg, live);
            }
        }
        PlanOp::CallProcedure { args, .. } => {
            for arg in args {
                add_expr_vars_to_live(arg, live);
            }
        }
        PlanOp::InlineProcedureCall { scope, .. } => {
            if let Some(vars) = scope.explicit_vars() {
                for variable in vars {
                    live.insert(variable.to_string());
                }
            }
        }
        PlanOp::WorstCaseOptimalJoin { variables, edges } => {
            for variable in variables {
                live.remove(variable.as_ref());
            }
            for edge in edges {
                live.remove(edge.variable.as_ref());
                if let Some(hop_aux_binding) = &edge.hop_aux_binding {
                    live.remove(hop_aux_binding.as_ref());
                }
            }
        }
        PlanOp::SetProperties { items } => {
            for item in items {
                match item {
                    crate::plan::SetPlanItem::Property {
                        variable, value, ..
                    } => {
                        live.insert(variable.to_string());
                        add_expr_vars_to_live(value, live);
                    }
                    crate::plan::SetPlanItem::AllProperties { variable, value } => {
                        live.insert(variable.to_string());
                        add_expr_vars_to_live(value, live);
                    }
                    crate::plan::SetPlanItem::Label { variable, .. } => {
                        live.insert(variable.to_string());
                    }
                }
            }
        }
        PlanOp::RemoveProperties { items } => {
            for item in items {
                match item {
                    crate::plan::RemovePlanItem::Property { variable, .. }
                    | crate::plan::RemovePlanItem::Label { variable, .. } => {
                        live.insert(variable.to_string());
                    }
                }
            }
        }
        PlanOp::DeleteVertex { variable }
        | PlanOp::DetachDeleteVertex { variable }
        | PlanOp::DeleteEdge { variable } => {
            live.insert(variable.to_string());
        }
        _ => {}
    }
}

fn add_expr_vars_to_live(expr: &gleaph_gql::ast::Expr, live: &mut LiveBindings) {
    if live.all {
        return;
    }
    collect_variables_ref(expr, &mut |v| {
        live.insert(v.to_string());
    });
}

fn exprs_reference_var(exprs: &[gleaph_gql::ast::Expr], var: &str) -> bool {
    exprs.iter().any(|expr| {
        let mut found = false;
        collect_variables_ref(expr, &mut |v| {
            if v == var {
                found = true;
            }
        });
        found
    })
}

#[cfg(test)]
mod tests {
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql::types::EdgeDirection;

    use crate::plan::{
        PlanAnnotations, PlanOp, ProjectColumn, ShortestMode, ShortestPathCost, YieldColumn,
    };

    use super::apply_shortest_path_binding_pruning;

    fn var(name: &str) -> Expr {
        Expr::new(ExprKind::Variable(name.to_owned()))
    }

    fn shortest_path(edge: &str, path_var: Option<&str>) -> PlanOp {
        PlanOp::ShortestPath {
            src: "a".into(),
            dst: "b".into(),
            edge: edge.into(),
            path_var: path_var.map(Into::into),
            emit_edge_binding: true,
            emit_path_binding: true,
            mode: ShortestMode::AnyShortest,
            direction: EdgeDirection::PointingRight,
            label: None,
            label_expr: None,
            var_len: None,
            cost: ShortestPathCost::HopCount,
        }
    }

    fn shortest_path_flags(op: &PlanOp) -> (bool, bool) {
        match op {
            PlanOp::ShortestPath {
                emit_edge_binding,
                emit_path_binding,
                ..
            } => (*emit_edge_binding, *emit_path_binding),
            other => panic!("expected ShortestPath, got {other:?}"),
        }
    }

    #[test]
    fn shortest_path_pruning_keeps_bindings_for_project_star() {
        let mut ops = vec![
            shortest_path("e", Some("p")),
            PlanOp::Project {
                columns: Vec::new(),
                distinct: false,
            },
        ];

        apply_shortest_path_binding_pruning(&mut ops, &mut PlanAnnotations::default());

        assert_eq!(shortest_path_flags(&ops[0]), (true, true));
    }

    #[test]
    fn shortest_path_pruning_keeps_edge_used_by_call_argument() {
        let mut ops = vec![
            shortest_path("e", Some("p")),
            PlanOp::CallProcedure {
                name: vec!["db".into(), "echo".into()],
                args: vec![var("e")],
                yield_columns: Some(vec![YieldColumn {
                    name: "x".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: var("x"),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        apply_shortest_path_binding_pruning(&mut ops, &mut PlanAnnotations::default());

        assert_eq!(shortest_path_flags(&ops[0]), (true, false));
    }

    #[test]
    fn shortest_path_pruning_keeps_edge_used_as_hash_join_key() {
        let mut ops = vec![
            PlanOp::HashJoin {
                left: vec![shortest_path("e", Some("p"))],
                right: vec![PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: var("e"),
                        alias: None,
                    }],
                    distinct: false,
                }],
                join_keys: vec!["e".into()],
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: var("a"),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        apply_shortest_path_binding_pruning(&mut ops, &mut PlanAnnotations::default());

        let PlanOp::HashJoin { left, .. } = &ops[0] else {
            panic!("expected HashJoin");
        };
        assert_eq!(shortest_path_flags(&left[0]), (true, false));
    }

    fn expand(edge: &str) -> PlanOp {
        PlanOp::Expand {
            src: "a".into(),
            edge: edge.into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: None,
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
        }
    }

    fn expand_filter(edge: &str, dst_filter: Vec<Expr>) -> PlanOp {
        PlanOp::ExpandFilter {
            src: "a".into(),
            edge: edge.into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: None,
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            dst_filter,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        }
    }

    fn expand_emit_flag(op: &PlanOp) -> bool {
        match op {
            PlanOp::Expand {
                emit_edge_binding, ..
            }
            | PlanOp::ExpandFilter {
                emit_edge_binding, ..
            } => *emit_edge_binding,
            _ => panic!("expected Expand op"),
        }
    }

    #[test]
    fn expand_pruning_return_dst_only() {
        let mut ops = vec![
            expand("e"),
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: var("b"),
                    alias: None,
                }],
                distinct: false,
            },
        ];
        apply_shortest_path_binding_pruning(&mut ops, &mut PlanAnnotations::default());
        assert!(!expand_emit_flag(&ops[0]));
    }

    #[test]
    fn expand_filter_pruning_keeps_edge_when_dst_filter_reads_edge() {
        let mut ops = vec![
            expand_filter(
                "e",
                vec![Expr::new(ExprKind::IsNotNull(Box::new(var("e"))))],
            ),
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: var("b"),
                    alias: None,
                }],
                distinct: false,
            },
        ];
        apply_shortest_path_binding_pruning(&mut ops, &mut PlanAnnotations::default());
        assert!(expand_emit_flag(&ops[0]));
    }
}
