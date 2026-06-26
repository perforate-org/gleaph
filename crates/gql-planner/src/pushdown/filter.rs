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

    // Apply moves (in reverse order to preserve indices).
    moves.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (from, to) in &moves {
        let op = ops.remove(*from);
        ops.insert(*to, op);
    }

    if !moves.is_empty() {
        annotations.optimizer.filter_pushdown_stages = moves.iter().map(|(_, to)| *to).collect();
    }
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
