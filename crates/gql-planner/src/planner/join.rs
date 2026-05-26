use gleaph_gql::ast::*;
use gleaph_gql::type_check::{BindingKind, PropertySchema};
use std::collections::{BTreeMap, BTreeSet};

use crate::path_extensions::PlanBuildOptions;
use crate::plan::*;
use crate::pushdown;
use crate::stats::GraphStats;
use super::PlannerError;
// ════════════════════════════════════════════════════════════════════════════════
// Bushy Join Detection
// ════════════════════════════════════════════════════════════════════════════════

/// Detect independent MATCH groups by analyzing variable flow.
/// Returns groups of part indices. If all parts are dependent, returns a single group.
///
/// Uses a variable→part-index hash map for O(V + N) grouping instead of O(N²)
/// pairwise comparison, where V = total variable count and N = number of parts.
pub(super) fn detect_independent_match_groups(parts: &[SimpleQueryStatement]) -> Vec<Vec<usize>> {
    let n = parts.len();
    let mut uf = UnionFind::new(n);

    // Map: variable name → first part index that mentions it.
    let mut var_first_part: rapidhash::RapidHashMap<String, usize> =
        rapidhash::RapidHashMap::default();

    for (i, part) in parts.iter().enumerate() {
        let mut vars = std::collections::BTreeSet::new();
        if let SimpleQueryStatement::Match(m) = part {
            collect_pattern_variables(&m.pattern, &mut vars);
        }

        for var in vars {
            match var_first_part.entry(var) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    // Variable seen before: merge groups.
                    uf.union(*e.get(), i);
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(i);
                }
            }
        }

        // Non-MATCH parts are always dependent on the previous MATCH.
        if !matches!(part, SimpleQueryStatement::Match(_)) && i > 0 {
            uf.union(i - 1, i);
        }
    }

    // Collect groups preserving order.
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        groups.entry(uf.find(i)).or_default().push(i);
    }

    groups.into_values().collect()
}

/// Lightweight union-find with path compression and union by rank.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        // Path splitting (iterative path compression).
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        // Union by rank.
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

/// Collect all pattern variables from a GraphPattern.
fn collect_pattern_variables(
    pattern: &GraphPattern,
    vars: &mut std::collections::BTreeSet<String>,
) {
    for path in &pattern.paths {
        collect_path_expr_variables(&path.expr, vars);
        if let Some(v) = &path.variable {
            vars.insert(v.clone());
        }
    }
    // WHERE clause references.
    if let Some(where_expr) = &pattern.where_clause {
        for v in pushdown::collect_variables(where_expr) {
            vars.insert(v);
        }
    }
}

fn collect_path_expr_variables(
    expr: &PathPatternExpr,
    vars: &mut std::collections::BTreeSet<String>,
) {
    match expr {
        PathPatternExpr::Term(term) => {
            for factor in &term.factors {
                match &factor.primary {
                    PathPrimary::Node(node) => {
                        if let Some(v) = &node.variable {
                            vars.insert(v.clone());
                        }
                    }
                    PathPrimary::Edge(edge) => {
                        if let Some(v) = &edge.variable {
                            vars.insert(v.clone());
                        }
                    }
                    PathPrimary::Parenthesized { expr, .. } => {
                        collect_path_expr_variables(expr, vars);
                    }
                    _ => {}
                }
            }
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                for factor in &term.factors {
                    match &factor.primary {
                        PathPrimary::Node(node) => {
                            if let Some(v) = &node.variable {
                                vars.insert(v.clone());
                            }
                        }
                        PathPrimary::Edge(edge) => {
                            if let Some(v) = &edge.variable {
                                vars.insert(v.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Build bushy join plan for independent MATCH groups.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_bushy_join(
    groups: &[Vec<usize>],
    parts: &[SimpleQueryStatement],
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    referenced_vars: &BTreeSet<String>,
    schema: &dyn PropertySchema,
    options: PlanBuildOptions<'_>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    let mut sub_plans: Vec<Vec<PlanOp>> = Vec::new();
    let mut sub_vars: Vec<std::collections::BTreeSet<String>> = Vec::new();

    for group in groups {
        let mut group_ops = Vec::new();
        let mut group_vars = std::collections::BTreeSet::new();
        let mut bound_node_vars = BTreeSet::new();
        let mut optional_node_vars = BTreeSet::new();

        for &idx in group {
            super::match_plan::plan_simple_statement(
                &parts[idx],
                idx,
                stats,
                conditional_candidates,
                binding_kinds,
                referenced_vars,
                schema,
                options,
                &mut bound_node_vars,
                &mut optional_node_vars,
                &mut group_ops,
                annotations,
            )?;
            if let SimpleQueryStatement::Match(m) = &parts[idx] {
                collect_pattern_variables(&m.pattern, &mut group_vars);
            }
        }

        sub_vars.push(group_vars);
        sub_plans.push(group_ops);
    }

    // Join sub-plans pairwise.
    let mut result_ops = sub_plans.remove(0);
    let mut result_vars = sub_vars.remove(0);

    for (plan, vars) in sub_plans.into_iter().zip(sub_vars) {
        let shared: Vec<String> = result_vars.intersection(&vars).cloned().collect();

        let left = std::mem::take(&mut result_ops);
        if shared.is_empty() {
            result_ops = vec![PlanOp::CartesianProduct { left, right: plan }];
        } else {
            let join_keys: Vec<Str> = shared.iter().map(|s| Str::from(s.as_str())).collect();
            result_ops = vec![PlanOp::HashJoin {
                left,
                right: plan,
                join_keys,
            }];
        }

        result_vars.extend(vars);
    }

    ops.extend(result_ops);
    Ok(())
}
