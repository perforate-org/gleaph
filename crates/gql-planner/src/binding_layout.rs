//! Execution-time variable layout: stable slot indices for [`PlanRow`] bindings.
//!
//! Derived from plan ops so the graph executor can store rows as dense vectors instead
//! of `BTreeMap` when variables are known statically.

use std::collections::HashMap;
use std::rc::Rc;

use crate::plan::{PlanOp, Str};

/// Maps plan variable names to dense `0..len` indices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BindingLayout {
    names: Vec<Str>,
    index: HashMap<String, u32>,
}

impl BindingLayout {
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn names(&self) -> &[Str] {
        &self.names
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.index.get(name).map(|&i| i as usize)
    }

    pub fn name_at(&self, index: usize) -> Option<&str> {
        self.names.get(index).map(|s| s.as_ref())
    }

    pub fn single(name: Str) -> Self {
        let mut layout = Self::default();
        layout.insert_name(name);
        layout
    }

    fn insert_name(&mut self, name: Str) -> usize {
        if let Some(&idx) = self.index.get(name.as_ref()) {
            return idx as usize;
        }
        let idx = self.names.len();
        self.index.insert(name.to_string(), idx as u32);
        self.names.push(name);
        idx
    }
}

/// Collect variables that may appear on a row during execution of a linear op list.
pub fn derive_binding_layout(ops: &[PlanOp]) -> BindingLayout {
    let mut layout = BindingLayout::default();
    for op in ops {
        register_op_bindings(op, &mut layout);
    }
    layout
}

fn register_op_bindings(op: &PlanOp, layout: &mut BindingLayout) {
    match op {
        PlanOp::NodeScan { variable, .. }
        | PlanOp::IndexScan { variable, .. }
        | PlanOp::EdgeIndexScan { variable, .. }
        | PlanOp::IndexIntersection { variable, .. } => {
            layout.insert_name(variable.clone());
        }
        PlanOp::ConditionalIndexScan {
            fallback_variable,
            candidates,
            ..
        } => {
            layout.insert_name(fallback_variable.clone());
            for c in candidates {
                layout.insert_name(c.variable.clone());
            }
        }
        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            hop_aux_binding,
            ..
        } => {
            layout.insert_name(edge.clone());
            layout.insert_name(near.clone());
            layout.insert_name(far.clone());
            if let Some(hop) = hop_aux_binding {
                layout.insert_name(hop.clone());
            }
        }
        PlanOp::Expand {
            src,
            edge,
            dst,
            hop_aux_binding,
            emit_edge_binding,
            ..
        }
        | PlanOp::ExpandFilter {
            src,
            edge,
            dst,
            hop_aux_binding,
            emit_edge_binding,
            ..
        } => {
            layout.insert_name(src.clone());
            if *emit_edge_binding {
                layout.insert_name(edge.clone());
            }
            layout.insert_name(dst.clone());
            if let Some(hop) = hop_aux_binding {
                layout.insert_name(hop.clone());
            }
        }
        PlanOp::ShortestPath {
            src,
            dst,
            edge,
            path_var,
            emit_edge_binding,
            emit_path_binding,
            ..
        } => {
            layout.insert_name(src.clone());
            layout.insert_name(dst.clone());
            if *emit_edge_binding {
                layout.insert_name(edge.clone());
            }
            if *emit_path_binding && let Some(path_var) = path_var {
                layout.insert_name(path_var.clone());
            }
        }
        PlanOp::Let { bindings } => {
            for binding in bindings {
                layout.insert_name(Str::from(binding.variable.as_str()));
            }
        }
        PlanOp::For {
            variable,
            ordinality,
            ..
        } => {
            layout.insert_name(variable.clone());
            if let Some(ord) = ordinality {
                layout.insert_name(ord.clone());
            }
        }
        PlanOp::Project { .. } | PlanOp::Materialize { .. } => {}
        PlanOp::OptionalMatch { sub_plan } => {
            register_subplan_bindings(sub_plan, layout);
        }
        PlanOp::HashJoin {
            left,
            right,
            join_keys,
            ..
        } => {
            register_subplan_bindings(left, layout);
            register_subplan_bindings(right, layout);
            for key in join_keys {
                layout.insert_name(key.clone());
            }
        }
        PlanOp::CartesianProduct { left, right } => {
            register_subplan_bindings(left, layout);
            register_subplan_bindings(right, layout);
        }
        PlanOp::SetOperation { right, .. } => {
            register_subplan_bindings(&right.ops, layout);
        }
        PlanOp::InlineProcedureCall {
            sub_plan,
            scope_vars,
            ..
        } => {
            register_subplan_bindings(&sub_plan.ops, layout);
            for v in scope_vars {
                layout.insert_name(v.clone());
            }
        }
        PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => register_subplan_bindings(sub_plan, layout),
        PlanOp::WorstCaseOptimalJoin {
            variables, edges, ..
        } => {
            for v in variables {
                layout.insert_name(v.clone());
            }
            for e in edges {
                layout.insert_name(e.src.clone());
                layout.insert_name(e.dst.clone());
                layout.insert_name(e.variable.clone());
            }
        }
        PlanOp::InsertVertex { variable, .. } => {
            if let Some(variable) = variable {
                layout.insert_name(variable.clone());
            }
        }
        PlanOp::InsertEdge {
            variable, src, dst, ..
        } => {
            if let Some(variable) = variable {
                layout.insert_name(variable.clone());
            }
            layout.insert_name(src.clone());
            layout.insert_name(dst.clone());
        }
        PlanOp::SetProperties { items } => register_set_items(items, layout),
        PlanOp::RemoveProperties { items } => register_remove_items(items, layout),
        PlanOp::DeleteVertex { variable }
        | PlanOp::DetachDeleteVertex { variable }
        | PlanOp::DeleteEdge { variable } => {
            layout.insert_name(variable.clone());
        }
        PlanOp::Filter { .. }
        | PlanOp::PropertyFilter { .. }
        | PlanOp::Sort { .. }
        | PlanOp::Limit { .. }
        | PlanOp::TopK { .. }
        | PlanOp::Aggregate { .. }
        | PlanOp::CallProcedure { .. }
        | PlanOp::UseGraph { sub_plan: None, .. } => {}
    }
}

fn register_subplan_bindings(ops: &[PlanOp], layout: &mut BindingLayout) {
    for op in ops {
        register_op_bindings(op, layout);
    }
}

fn register_set_items(items: &[crate::plan::SetPlanItem], layout: &mut BindingLayout) {
    for item in items {
        match item {
            crate::plan::SetPlanItem::Property { variable, .. }
            | crate::plan::SetPlanItem::AllProperties { variable, .. }
            | crate::plan::SetPlanItem::Label { variable, .. } => {
                layout.insert_name(variable.clone());
            }
        }
    }
}

fn register_remove_items(items: &[crate::plan::RemovePlanItem], layout: &mut BindingLayout) {
    for item in items {
        match item {
            crate::plan::RemovePlanItem::Property { variable, .. }
            | crate::plan::RemovePlanItem::Label { variable, .. } => {
                layout.insert_name(variable.clone());
            }
        }
    }
}

/// Shared layout handle attached to each indexed row in a plan execution.
pub type SharedBindingLayout = Rc<BindingLayout>;

#[cfg(test)]
mod tests {
    use crate::plan::{PhysicalPlan, PlanOp, ProjectColumn, ShortestMode};
    use gleaph_gql::ast::{Expr, ExprKind};

    #[test]
    fn all_shortest_bench_layout_includes_shortest_path_variables() {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Src".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("Dst".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: false,
                emit_path_binding: true,
                mode: ShortestMode::AllShortest,
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: None,
                label_expr: None,
                var_len: None,
                cost: crate::plan::ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("p".into())),
                    alias: Some("p".into()),
                }],
                distinct: false,
            },
        ]);
        assert!(plan.binding_layout.index_of("a").is_some());
        assert!(plan.binding_layout.index_of("c").is_some());
        assert!(plan.binding_layout.index_of("p").is_some());
    }
}
