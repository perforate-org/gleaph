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
                    edge_value_predicate,
                    edge_vector_predicate,
                    edge_property_projection,
                    dst_property_projection,
                    hop_aux_binding,
                    emit_edge_binding,
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
                        edge_value_predicate,
                        edge_vector_predicate,
                        dst_filter: predicates,
                        edge_property_projection,
                        dst_property_projection,
                        hop_aux_binding,
                        emit_edge_binding,
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

/// Prune unnecessary `ShortestPath` output bindings when downstream ops don't read them.
pub fn apply_shortest_path_binding_pruning(
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
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
                ..
            } => {
                *emit_edge_binding =
                    live.contains(edge.as_ref()) || exprs_reference_var(dst_filter, edge.as_ref());
            }
            PlanOp::Expand {
                edge,
                emit_edge_binding,
                ..
            } => {
                *emit_edge_binding = live.contains(edge.as_ref());
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
        PlanOp::InlineProcedureCall { scope_vars, .. } => {
            for variable in scope_vars {
                live.insert(variable.to_string());
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
            edge_value_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
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
            edge_value_predicate: None,
            edge_vector_predicate: None,
            dst_filter,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
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
