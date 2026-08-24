use rapidhash::RapidHashSet;

use crate::plan::{PlanAnnotations, PlanOp};

use super::vars::collect_variables_ref;

pub fn apply_filter_pushdown(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    // Build variable availability by position: which variables are produced
    // by ops[0..=i].
    let var_sets = produced_vars_by_position(ops);

    // Collect PropertyFilter indices and check if they can be moved earlier.
    let mut moves: Vec<(usize, usize)> = Vec::new(); // (from, to)

    for (idx, op) in ops.iter().enumerate() {
        if let PlanOp::PropertyFilter { predicates, .. } = op {
            // Collect all variables referenced by all predicates (zero-copy).
            let mut referenced = RapidHashSet::default();
            for pred in predicates {
                collect_variables_ref(pred, &mut |v| {
                    referenced.insert(v.to_string());
                });
            }

            // Find the earliest position where all referenced vars are available.
            let earliest = (0..var_sets.len())
                .find(|&i| var_sets.contains_all(i, &referenced))
                .unwrap_or(idx);

            // +1 because we want to place after the op that produces the last var.
            let mut target = earliest + 1;

            // Don't push past a Materialize barrier.
            let last_materialize = ops[..idx]
                .iter()
                .rposition(|op| matches!(op, PlanOp::Materialize { .. }));
            if let Some(mat_idx) = last_materialize {
                target = target.max(mat_idx + 1);
            }

            if target < idx {
                moves.push((idx, target));
            }
        }
    }

    if moves.is_empty() {
        return;
    }

    // Rebuild in one pass against original positions. Applying `(from, to)` pairs
    // sequentially shifts every later index, so a second move can drag an unrelated
    // filter across its producer op (filters have been observed landing before the
    // `EdgeBindEndpoints` that binds their variable). Anchors (`to - 1`) are scan /
    // bind ops, which are never moved themselves, so queueing each move after its
    // anchor keeps every target stable.
    let mut taken = vec![false; ops.len()];
    for (from, _) in &moves {
        taken[*from] = true;
    }
    let mut queues: Vec<Vec<(usize, PlanOp)>> = vec![Vec::new(); ops.len()];
    for (from, to) in &moves {
        queues[*to - 1].push((*from, ops[*from].clone()));
    }
    let mut out = Vec::with_capacity(ops.len());
    for i in 0..ops.len() {
        if !taken[i] {
            out.push(ops[i].clone());
        }
        let mut slot = std::mem::take(&mut queues[i]);
        slot.sort_by_key(|(from, _)| *from);
        out.extend(slot.into_iter().map(|(_, op)| op));
    }
    *ops = out;

    annotations.optimizer.filter_pushdown_stages = moves.iter().map(|(_, to)| *to).collect();
}

/// Compute which variables are available after each op in the plan.
/// Uses a single cumulative `RapidHashSet` and shares it via `Rc` to avoid cloning.
fn produced_vars_by_position(ops: &[PlanOp]) -> ProducedVars {
    let mut pv = ProducedVars::new(ops.len());
    let mut current = RapidHashSet::default();

    for op in ops {
        match op {
            PlanOp::NodeScan { variable, .. } | PlanOp::IndexScan { variable, .. } => {
                current.insert(variable.to_string());
            }
            PlanOp::EdgeIndexScan { variable, .. } => {
                current.insert(variable.to_string());
            }
            PlanOp::ConditionalIndexScan {
                fallback_variable, ..
            } => {
                current.insert(fallback_variable.to_string());
            }
            PlanOp::IndexIntersection { variable, .. } => {
                current.insert(variable.to_string());
            }
            PlanOp::Expand {
                src,
                edge,
                dst,
                hop_aux_binding,
                ..
            }
            | PlanOp::ExpandFilter {
                src,
                edge,
                dst,
                hop_aux_binding,
                ..
            } => {
                current.insert(src.to_string());
                current.insert(edge.to_string());
                current.insert(dst.to_string());
                if let Some(h) = hop_aux_binding {
                    current.insert(h.to_string());
                }
            }
            PlanOp::EdgeBindEndpoints {
                edge,
                near,
                far,
                hop_aux_binding,
                ..
            } => {
                current.insert(edge.to_string());
                current.insert(near.to_string());
                current.insert(far.to_string());
                if let Some(h) = hop_aux_binding {
                    current.insert(h.to_string());
                }
            }
            PlanOp::ShortestPath {
                src,
                dst,
                edge,
                path_var,
                ..
            } => {
                current.insert(src.to_string());
                current.insert(dst.to_string());
                current.insert(edge.to_string());
                if let Some(pv) = path_var {
                    current.insert(pv.to_string());
                }
            }
            PlanOp::Let { bindings } => {
                for b in bindings {
                    current.insert(b.variable.clone());
                }
            }
            PlanOp::For {
                variable,
                ordinality,
                ..
            } => {
                current.insert(variable.to_string());
                if let Some(ord) = ordinality {
                    current.insert(ord.to_string());
                }
            }
            PlanOp::Search { output, .. } => {
                current.insert(output.alias.to_string());
            }
            PlanOp::InsertVertex {
                variable: Some(v), ..
            }
            | PlanOp::InsertEdge {
                variable: Some(v), ..
            } => {
                current.insert(v.to_string());
            }
            PlanOp::CallProcedure {
                yield_columns: Some(cols),
                ..
            } => {
                for col in cols {
                    current.insert(col.alias.as_deref().unwrap_or(&col.name).to_string());
                }
            }
            PlanOp::InlineProcedureCall { scope, .. } => {
                if let Some(vars) = scope.explicit_vars() {
                    for v in vars {
                        current.insert(v.to_string());
                    }
                }
            }
            PlanOp::UseGraph {
                sub_plan: Some(sub_ops),
                ..
            } => {
                let sub_pv = produced_vars_by_position(sub_ops);
                if let Some(last) = sub_pv.last() {
                    current.extend(last.iter().cloned());
                }
            }
            PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
                let left_pv = produced_vars_by_position(left);
                let right_pv = produced_vars_by_position(right);
                if let Some(l) = left_pv.last() {
                    current.extend(l.iter().cloned());
                }
                if let Some(r) = right_pv.last() {
                    current.extend(r.iter().cloned());
                }
            }
            PlanOp::WorstCaseOptimalJoin { variables, edges } => {
                for v in variables {
                    current.insert(v.to_string());
                }
                for e in edges {
                    current.insert(e.variable.to_string());
                    if let Some(h) = &e.hop_aux_binding {
                        current.insert(h.to_string());
                    }
                }
            }
            PlanOp::Materialize { columns, .. } => {
                current.clear();
                for col in columns {
                    if let Some(alias) = &col.alias {
                        current.insert(alias.to_string());
                    } else {
                        collect_variables_ref(&col.expr, &mut |v| {
                            current.insert(v.to_string());
                        });
                    }
                }
            }
            _ => {}
        }
        pv.push_snapshot(&current);
    }

    pv
}

/// Compact representation of per-position variable sets.
/// Stores a flat Vec of variable names with index boundaries to avoid
/// cloning BTreeSet/HashSet at every position.
struct ProducedVars {
    /// All variable names, concatenated.
    vars: Vec<String>,
    /// `boundaries[i]` is the end index in `vars` for position `i`.
    /// Position `i` has variables `vars[start..boundaries[i]]`
    /// where `start = if i == 0 { 0 } else { boundaries[i-1] }`.
    boundaries: Vec<usize>,
}

impl ProducedVars {
    fn new(capacity: usize) -> Self {
        Self {
            vars: Vec::with_capacity(capacity * 4),
            boundaries: Vec::with_capacity(capacity),
        }
    }

    /// Snapshot the current cumulative set at this position.
    fn push_snapshot(&mut self, current: &RapidHashSet<String>) {
        // Append all variables in the cumulative set.
        for v in current {
            self.vars.push(v.clone());
        }
        self.boundaries.push(self.vars.len());
    }

    /// Get the variable set at position `i`.
    fn at(&self, i: usize) -> &[String] {
        let start = if i == 0 { 0 } else { self.boundaries[i - 1] };
        let end = self.boundaries[i];
        &self.vars[start..end]
    }

    /// Check if all given variables are available at position `i`.
    fn contains_all(&self, i: usize, vars: &RapidHashSet<String>) -> bool {
        let slice = self.at(i);
        vars.iter().all(|v| slice.iter().any(|s| s == v))
    }

    fn last(&self) -> Option<&[String]> {
        if self.boundaries.is_empty() {
            None
        } else {
            Some(self.at(self.boundaries.len() - 1))
        }
    }

    fn len(&self) -> usize {
        self.boundaries.len()
    }
}

/// Check if a filter predicate references only variables available at a given
/// stage. This enables filter pushdown to earlier stages.
pub fn can_push_filter_to_stage(
    predicate: &gleaph_gql::ast::Expr,
    available_vars: &[String],
) -> bool {
    let mut result = true;
    collect_variables_ref(predicate, &mut |v| {
        if !available_vars.contains(&v.to_string()) {
            result = false;
        }
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
    use gleaph_gql::types::{EdgeDirection, LabelExpr};
    use std::rc::Rc;

    fn labeled_pred(var: &str, label: &str) -> Expr {
        Expr::new(ExprKind::IsLabeled {
            expr: Box::new(Expr::var(var)),
            label: LabelExpr::Name(label.to_string()),
            negated: false,
        })
    }

    fn edge_range_pred(var: &str, property: &str, value: i64) -> Expr {
        Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::var(var)),
                property: property.to_string(),
            })),
            op: CmpOp::Ge,
            right: Box::new(Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(
                value,
            )))),
        })
    }

    /// Regression: with a residual one-sided range predicate present, two moves in
    /// one pass applied sequentially used stale indices and dragged `IsLabeled(b)`
    /// ahead of the `EdgeBindEndpoints` that binds `b`. Every PropertyFilter must
    /// sit after every op that produces each variable it references.
    #[test]
    fn filter_pushdown_never_moves_filters_before_their_producer() {
        let ops = vec![
            PlanOp::EdgeIndexScan {
                variable: Rc::from("e"),
                property: Rc::from("weight"),
                value: crate::plan::ScanValue::Literal(gleaph_gql::Value::Int64(7)),
                cmp: CmpOp::Ge,
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: Rc::from("e"),
                near: Rc::from("a"),
                far: Rc::from("b"),
                direction: EdgeDirection::PointingRight,
                label: None,
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![labeled_pred("b", "B")],
                stage: 0,
            },
            PlanOp::PropertyFilter {
                predicates: vec![labeled_pred("a", "A")],
                stage: 0,
            },
            PlanOp::PropertyFilter {
                predicates: vec![edge_range_pred("e", "weight", 7)],
                stage: 0,
            },
        ];
        let mut ops = ops;
        let mut annotations = PlanAnnotations::default();
        apply_filter_pushdown(&mut ops, &mut annotations);

        // The residual range predicate moves next to the scan; both endpoint label
        // checks stay after the bind that introduces their variables.
        let bind_pos = ops
            .iter()
            .position(|op| matches!(op, PlanOp::EdgeBindEndpoints { .. }))
            .expect("bind endpoints retained");
        for (i, op) in ops.iter().enumerate() {
            let PlanOp::PropertyFilter { predicates, .. } = op else {
                continue;
            };
            let referenced: std::collections::BTreeSet<String> = predicates
                .iter()
                .flat_map(|pred| {
                    let mut vars = Vec::new();
                    collect_variables_ref(pred, &mut |v| vars.push(v.to_string()));
                    vars
                })
                .collect();
            let produced: std::collections::BTreeSet<String> = ops[..i]
                .iter()
                .flat_map(|op| -> Vec<String> {
                    match op {
                        PlanOp::EdgeIndexScan { variable, .. } => vec![variable.to_string()],
                        PlanOp::EdgeBindEndpoints {
                            edge,
                            near,
                            far,
                            hop_aux_binding,
                            ..
                        } => {
                            let mut vars =
                                vec![edge.to_string(), near.to_string(), far.to_string()];
                            if let Some(h) = hop_aux_binding {
                                vars.push(h.to_string());
                            }
                            vars
                        }
                        _ => Vec::new(),
                    }
                })
                .collect();
            assert!(
                referenced.is_subset(&produced),
                "filter at {i} references {referenced:?} before its producer; ops={ops:?}"
            );
        }
        assert!(
            bind_pos < ops.len(),
            "bind endpoints must remain in the plan"
        );
    }
}
