//! Filter and limit pushdown optimizations.
//!
//! These optimizations move operators earlier in the pipeline when safe,
//! reducing the number of rows that flow through expensive later stages.

use rapidhash::RapidHashSet;

use gleaph_gql::ast::ExprKind;

use crate::expr_children::for_each_immediate_child_expr;
use crate::plan::{PlanAnnotations, PlanOp};

/// Apply filter pushdown: move PropertyFilter ops to the earliest stage where
/// all their referenced variables are available.
///
/// This reduces intermediate row counts by filtering as early as possible.
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
            PlanOp::InlineProcedureCall { scope_vars, .. } => {
                for v in scope_vars {
                    current.insert(v.to_string());
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

/// Apply limit pushdown: move LIMIT before Sort/Project when safe.
///
/// Safe conditions:
/// - No ORDER BY (Sort) between the scan and the Limit
/// - No Aggregate
/// - No DISTINCT in Project
pub fn apply_limit_pushdown(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    // Single scan to gather all needed info.
    let mut has_sort = false;
    let mut has_aggregate = false;
    let mut has_distinct = false;
    let mut limit_idx = None;
    let mut project_idx = None;
    for (i, op) in ops.iter().enumerate() {
        match op {
            PlanOp::Sort { .. } => has_sort = true,
            PlanOp::Aggregate { .. } => has_aggregate = true,
            PlanOp::Project { distinct: true, .. } => has_distinct = true,
            PlanOp::Limit { .. } => limit_idx = Some(i),
            PlanOp::Project { .. } => project_idx = Some(i),
            _ => {}
        }
    }

    if has_sort || has_aggregate || has_distinct {
        return;
    }

    if let (Some(li), Some(pi)) = (limit_idx, project_idx)
        && li > pi
    {
        // Move Limit before Project.
        let limit_op = ops.remove(li);
        ops.insert(pi, limit_op);
        annotations.optimizer.limit_pushdown_applied = true;
    }
}

/// TopK fusion: when Sort is immediately followed by Limit (possibly with
/// a Project in between), fuse them into a single TopK operator.
pub fn apply_topk_fusion(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    // Look for Sort followed by Limit (with optional Project in between).
    let sort_idx = ops.iter().rposition(|op| matches!(op, PlanOp::Sort { .. }));
    let limit_idx = ops
        .iter()
        .rposition(|op| matches!(op, PlanOp::Limit { count: Some(_), .. }));

    if let (Some(si), Some(li)) = (sort_idx, limit_idx) {
        // Limit must come after Sort, and there should be no Aggregate between them.
        if li > si
            && !ops[si..li]
                .iter()
                .any(|op| matches!(op, PlanOp::Aggregate { .. }))
        {
            // Extract Sort and Limit.
            let limit_op = ops.remove(li);
            let sort_op = ops.remove(si);

            if let (
                PlanOp::Sort { order_by },
                PlanOp::Limit {
                    count: Some(k),
                    offset,
                },
            ) = (sort_op, limit_op)
            {
                ops.insert(
                    si,
                    PlanOp::TopK {
                        order_by,
                        k,
                        offset,
                    },
                );
                annotations.optimizer.topk_applied = true;
            }
        }
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

/// Predicate reordering: sort predicates within each PropertyFilter by
/// estimated selectivity (most selective first) for early short-circuit.
pub fn apply_predicate_reordering(
    ops: &mut [PlanOp],
    annotations: &mut PlanAnnotations,
    stats: Option<&dyn crate::stats::GraphStats>,
) {
    let mut reordered = false;
    for op in ops.iter_mut() {
        if let PlanOp::PropertyFilter { predicates, .. } = op
            && predicates.len() > 1
        {
            // Capture original order by pointer identity.
            let original_ptrs: Vec<*const gleaph_gql::ast::Expr> =
                predicates.iter().map(|p| p as *const _).collect();
            predicates.sort_by(|a, b| {
                let sa = crate::cost::estimate_predicate_selectivity(a, stats);
                let sb = crate::cost::estimate_predicate_selectivity(b, stats);
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            });
            let new_ptrs: Vec<*const gleaph_gql::ast::Expr> =
                predicates.iter().map(|p| p as *const _).collect();
            if original_ptrs != new_ptrs {
                reordered = true;
            }
        }
    }
    if reordered {
        annotations.optimizer.predicate_reordering_applied = true;
    }
}

/// EVFusion: fuse Expand + PropertyFilter into ExpandFilter when the filter
/// references only the destination variable of the preceding Expand.
pub fn apply_ev_fusion(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let mut i = 0;
    while i + 1 < ops.len() {
        let can_fuse = match (&ops[i], &ops[i + 1]) {
            (PlanOp::Expand { dst, .. }, PlanOp::PropertyFilter { predicates, .. }) => {
                // Check all predicates reference only the dst variable (zero-copy).
                predicates.iter().all(|pred| all_variables_eq(pred, dst))
            }
            _ => false,
        };

        if can_fuse {
            // Extract both ops.
            let filter_op = ops.remove(i + 1);
            let expand_op = ops.remove(i);

            if let (
                PlanOp::Expand {
                    src,
                    edge,
                    dst,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    edge_property_projection,
                    dst_property_projection,
                    hop_aux_binding,
                },
                PlanOp::PropertyFilter { predicates, .. },
            ) = (expand_op, filter_op)
            {
                ops.insert(
                    i,
                    PlanOp::ExpandFilter {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        dst_filter: predicates,
                        edge_property_projection,
                        dst_property_projection,
                        hop_aux_binding,
                    },
                );
                annotations.optimizer.ev_fusion_applied = true;
            }
            // Don't increment i — check the new ExpandFilter against next op.
        } else {
            i += 1;
        }
    }
}

/// LateProject: ensure Project appears after all Filter/ExpandFilter ops.
/// If Project is found before any filtering op, move it after the last one.
pub fn apply_late_project(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let project_idx = ops
        .iter()
        .position(|op| matches!(op, PlanOp::Project { .. }));
    let last_filter_idx = ops.iter().rposition(|op| {
        matches!(
            op,
            PlanOp::PropertyFilter { .. }
                | PlanOp::Filter { .. }
                | PlanOp::ExpandFilter { .. }
                | PlanOp::Expand { .. }
        )
    });

    if let (Some(pi), Some(fi)) = (project_idx, last_filter_idx) {
        if pi < fi {
            let project_op = ops.remove(pi);
            // fi shifted by -1 since we removed an element before it.
            ops.insert(fi, project_op);
            annotations.optimizer.late_project_applied = true;
        } else {
            annotations.optimizer.late_project_applied = true; // Already in the right place.
        }
    }
}

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
