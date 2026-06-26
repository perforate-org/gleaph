//! Output column schema for the final [`crate::plan::PhysicalPlan`] result.
//!
//! Populated after planning optimizations so executors can hydrate only RETURN
//! columns using binding-kind-specific fast paths.

use std::collections::HashMap;

use gleaph_gql::ast::{Expr, ExprKind};

use crate::plan::{PlanOp, ProjectColumn, Str};

/// How a result column should be hydrated from a runtime plan binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputBindingKind {
    Vertex,
    Edge,
    Path,
    RemoteVertex,
    /// Already a GQL [`Value`] after `Project` (expressions, literals, etc.).
    Scalar,
    /// Kind is not known statically; inspect the runtime binding.
    Dynamic,
}

/// One column in the query result row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputColumn {
    /// Name exposed in the result row (alias or expression label).
    pub name: Str,
    /// Preferred hydration strategy for this column.
    pub kind: OutputBindingKind,
    /// When the column is a bare variable reference, the plan variable name.
    pub source_var: Option<Str>,
}

/// Describes which bindings to hydrate and in what order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSchema {
    pub columns: Vec<OutputColumn>,
}

impl OutputSchema {
    /// When true, every binding key in each row is hydrated (legacy `Project` with no columns).
    #[inline]
    pub fn hydrates_all_row_bindings(&self) -> bool {
        self.columns.is_empty()
    }
}

/// Derive the output schema from the final top-level `Project` / `Materialize` op.
pub fn derive_output_schema(ops: &[PlanOp]) -> OutputSchema {
    let kinds = infer_binding_kinds(ops);
    let Some(project_cols) = final_project_columns(ops) else {
        return OutputSchema::default();
    };
    if project_cols.is_empty() {
        return OutputSchema::default();
    }
    OutputSchema {
        columns: project_cols
            .iter()
            .map(|col| output_column_from_project(col, &kinds))
            .collect(),
    }
}

fn final_project_columns(ops: &[PlanOp]) -> Option<&[ProjectColumn]> {
    for op in ops.iter().rev() {
        match op {
            PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
                return Some(columns.as_slice());
            }
            _ => {}
        }
    }
    None
}

fn output_column_from_project(
    col: &ProjectColumn,
    kinds: &HashMap<String, OutputBindingKind>,
) -> OutputColumn {
    let (name, source_var) = project_column_name_and_var(col);
    let kind = match source_var.as_ref().map(|v| v.as_ref()) {
        Some(var) => kinds
            .get(var)
            .copied()
            .unwrap_or(OutputBindingKind::Dynamic),
        None => OutputBindingKind::Scalar,
    };
    OutputColumn {
        name,
        kind,
        source_var,
    }
}

fn project_column_name_and_var(col: &ProjectColumn) -> (Str, Option<Str>) {
    if let Some(alias) = &col.alias {
        let source_var = match &col.expr.kind {
            ExprKind::Variable(v) => Some(Str::from(v.as_str())),
            _ => None,
        };
        return (alias.clone(), source_var);
    }
    if let ExprKind::Variable(v) = &col.expr.kind {
        let name = Str::from(v.as_str());
        return (name.clone(), Some(name));
    }
    (Str::from(expression_label(&col.expr)), None)
}

fn expression_label(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Variable(v) => v.clone(),
        ExprKind::PropertyAccess { expr, property } => {
            format!("{}.{}", expression_label(expr), property)
        }
        _ => "expr".to_owned(),
    }
}

fn infer_binding_kinds(ops: &[PlanOp]) -> HashMap<String, OutputBindingKind> {
    let mut kinds = HashMap::new();
    for op in ops {
        register_binding_kinds(op, &mut kinds);
    }
    kinds
}

fn register_binding_kinds(op: &PlanOp, kinds: &mut HashMap<String, OutputBindingKind>) {
    match op {
        PlanOp::NodeScan { variable, .. }
        | PlanOp::IndexScan { variable, .. }
        | PlanOp::EdgeIndexScan { variable, .. }
        | PlanOp::IndexIntersection { variable, .. } => {
            kinds.insert(variable.to_string(), OutputBindingKind::Vertex);
        }
        PlanOp::ConditionalIndexScan {
            fallback_variable,
            candidates,
            ..
        } => {
            kinds.insert(fallback_variable.to_string(), OutputBindingKind::Vertex);
            for candidate in candidates {
                kinds.insert(candidate.variable.to_string(), OutputBindingKind::Vertex);
            }
        }
        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            hop_aux_binding,
            ..
        } => {
            kinds.insert(edge.to_string(), OutputBindingKind::Edge);
            kinds.insert(near.to_string(), OutputBindingKind::Vertex);
            kinds.insert(far.to_string(), OutputBindingKind::Vertex);
            if let Some(hop) = hop_aux_binding {
                kinds.insert(hop.to_string(), OutputBindingKind::Scalar);
            }
        }
        PlanOp::Expand {
            edge,
            dst,
            emit_edge_binding,
            hop_aux_binding,
            var_len,
            path_var,
            emit_path_binding,
            near_group_var,
            far_group_var,
            ..
        }
        | PlanOp::ExpandFilter {
            edge,
            dst,
            emit_edge_binding,
            hop_aux_binding,
            var_len,
            path_var,
            emit_path_binding,
            near_group_var,
            far_group_var,
            ..
        } => {
            kinds.insert(dst.to_string(), OutputBindingKind::Vertex);
            if *emit_edge_binding {
                kinds.insert(edge.to_string(), OutputBindingKind::Edge);
            }
            if let Some(hop) = hop_aux_binding {
                kinds.insert(hop.to_string(), OutputBindingKind::Scalar);
            }
            if var_len.is_some() {
                if let Some(near) = near_group_var {
                    kinds.insert(near.to_string(), OutputBindingKind::Vertex);
                }
                if let Some(far) = far_group_var {
                    kinds.insert(far.to_string(), OutputBindingKind::Vertex);
                }
                if *emit_path_binding && let Some(path_var) = path_var {
                    kinds.insert(path_var.to_string(), OutputBindingKind::Path);
                }
            }
        }
        PlanOp::ShortestPath {
            edge,
            path_var,
            emit_edge_binding,
            emit_path_binding,
            ..
        } => {
            if *emit_edge_binding {
                kinds.insert(edge.to_string(), OutputBindingKind::Edge);
            }
            if *emit_path_binding && let Some(path_var) = path_var {
                kinds.insert(path_var.to_string(), OutputBindingKind::Path);
            }
        }
        PlanOp::Let { bindings } => {
            for binding in bindings {
                kinds.insert(binding.variable.clone(), OutputBindingKind::Scalar);
            }
        }
        PlanOp::Search { output, .. } => {
            kinds.insert(output.alias.to_string(), OutputBindingKind::Scalar);
        }
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right, .. } => {
            kinds.extend(infer_binding_kinds(left));
            kinds.extend(infer_binding_kinds(right));
        }
        PlanOp::OptionalMatch { sub_plan, .. } => {
            kinds.extend(infer_binding_kinds(sub_plan));
        }
        PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => {
            kinds.extend(infer_binding_kinds(sub_plan));
        }
        PlanOp::SetOperation { right, .. } => {
            kinds.extend(infer_binding_kinds(&right.ops));
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            kinds.extend(infer_binding_kinds(&sub_plan.ops));
        }
        PlanOp::WorstCaseOptimalJoin {
            variables, edges, ..
        } => {
            for variable in variables {
                kinds.insert(variable.to_string(), OutputBindingKind::Vertex);
            }
            for edge in edges {
                kinds.insert(edge.variable.to_string(), OutputBindingKind::Edge);
                if let Some(h) = &edge.hop_aux_binding {
                    kinds.insert(h.to_string(), OutputBindingKind::Scalar);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanOp, ShortestMode, ShortestPathCost, VarLenSpec};
    use gleaph_gql::ast::Expr;
    use gleaph_gql::types::EdgeDirection;

    #[test]
    fn derives_path_column_for_shortest_return() {
        let ops = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: None,
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
                direction: EdgeDirection::PointingRight,
                label: None,
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(4),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::var("p"),
                    alias: None,
                }],
                distinct: false,
            },
        ];
        let schema = derive_output_schema(&ops);
        assert_eq!(schema.columns.len(), 1);
        assert_eq!(schema.columns[0].name.as_ref(), "p");
        assert_eq!(schema.columns[0].kind, OutputBindingKind::Path);
    }
}
