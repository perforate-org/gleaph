//! Cost estimation for physical plans.
//!
//! Uses a simple additive cost model ported from gleaph-old. Each operator
//! contributes a cost based on its type and estimated input cardinality.

use crate::plan::{PlanOp, VarLenSpec};
use crate::stats::{self, GraphStats};

/// Estimate the total cost of a plan (arbitrary units).
pub fn estimate_cost(ops: &[PlanOp], stats: Option<&dyn GraphStats>) -> f64 {
    let mut total_cost = 0.0;
    let mut estimated_rows = estimate_initial_rows(ops, stats);

    for op in ops {
        let (op_cost, new_rows) = estimate_op_cost(op, estimated_rows, stats);
        total_cost += op_cost;
        estimated_rows = new_rows;
    }

    total_cost
}

/// Estimate the number of rows after the initial scan.
fn estimate_initial_rows(ops: &[PlanOp], stats: Option<&dyn GraphStats>) -> f64 {
    for op in ops {
        match op {
            PlanOp::NodeScan { label, .. } => {
                if let Some(label) = label
                    && let Some(stats) = stats
                    && let Some(card) = stats::label_cardinality_with_id(stats, label)
                {
                    return card as f64;
                }
                return 1000.0; // Default estimate.
            }
            PlanOp::IndexScan { .. } => return 10.0, // Index scans are very selective.
            PlanOp::EdgeIndexScan { .. } => return 10.0,
            PlanOp::ConditionalIndexScan { .. } => return 50.0,
            _ => {}
        }
    }
    1000.0
}

/// Estimate cost and output rows for a single operator.
fn estimate_op_cost(op: &PlanOp, input_rows: f64, stats: Option<&dyn GraphStats>) -> (f64, f64) {
    match op {
        PlanOp::NodeScan { label, .. } => {
            let rows = if let Some(label) = label {
                stats
                    .and_then(|s| stats::label_cardinality_with_id(s, label))
                    .map(|c| c as f64)
                    .unwrap_or(1000.0)
            } else {
                10000.0
            };
            (rows * stats::COST_SCAN_PER_ROW, rows)
        }

        PlanOp::IndexScan { .. } => {
            let rows = 10.0;
            (
                rows * stats::COST_SCAN_PER_ROW * stats::COST_INDEX_SEEK_FRACTION,
                rows,
            )
        }

        PlanOp::EdgeIndexScan { .. } => {
            let rows = 10.0;
            (
                rows * stats::COST_SCAN_PER_ROW * stats::COST_INDEX_SEEK_FRACTION,
                rows,
            )
        }

        PlanOp::EdgeBindEndpoints { .. } => {
            let rows = input_rows.max(1.0);
            (input_rows * stats::COST_EXPAND_MULTIPLIER * 0.02, rows)
        }

        PlanOp::ConditionalIndexScan { .. } => {
            let rows = 50.0;
            (
                rows * stats::COST_SCAN_PER_ROW * stats::COST_INDEX_SEEK_FRACTION,
                rows,
            )
        }

        PlanOp::PropertyFilter { predicates, .. } => {
            let selectivity: f64 = predicates
                .iter()
                .map(|p| estimate_predicate_selectivity(p, stats))
                .product();
            let output_rows = (input_rows * selectivity).max(1.0);
            (
                input_rows * stats::COST_FILTER_PER_ROW * predicates.len() as f64,
                output_rows,
            )
        }

        PlanOp::Filter { condition } => {
            let selectivity = estimate_predicate_selectivity(condition, stats);
            let output_rows = (input_rows * selectivity).max(1.0);
            (input_rows * stats::COST_FILTER_PER_ROW, output_rows)
        }

        PlanOp::Expand {
            var_len,
            indexed_edge_equality,
            ..
        } => {
            let degree = stats.and_then(|s| s.avg_degree()).unwrap_or(10.0);
            let multiplier = var_len_multiplier(var_len.as_ref(), degree);
            let idx_sel = indexed_edge_equality
                .as_ref()
                .and_then(|(prop, _)| stats.and_then(|s| s.property_selectivity(prop.as_ref())))
                .unwrap_or(1.0);
            let output_rows = input_rows * multiplier * idx_sel;
            (
                input_rows * stats::COST_EXPAND_MULTIPLIER * idx_sel,
                output_rows,
            )
        }

        PlanOp::ExpandFilter {
            var_len,
            indexed_edge_equality,
            dst_filter,
            ..
        } => {
            let degree = stats.and_then(|s| s.avg_degree()).unwrap_or(10.0);
            let multiplier = var_len_multiplier(var_len.as_ref(), degree);
            let idx_sel = indexed_edge_equality
                .as_ref()
                .and_then(|(prop, _)| stats.and_then(|s| s.property_selectivity(prop.as_ref())))
                .unwrap_or(1.0);
            let filter_sel: f64 = dst_filter
                .iter()
                .map(|p| estimate_predicate_selectivity(p, stats))
                .product();
            let output_rows = input_rows * multiplier * idx_sel * filter_sel;
            // Cheaper than separate Expand + Filter (no intermediate materialization).
            (
                input_rows * stats::COST_EXPAND_MULTIPLIER * 0.8 * idx_sel,
                output_rows,
            )
        }

        PlanOp::ShortestPath { .. } => (input_rows * stats::COST_SHORTEST_PER_ROW, input_rows),

        PlanOp::Let { .. } | PlanOp::For { .. } => {
            // LET doesn't change row count; FOR may expand.
            (input_rows * stats::COST_PROJECT_PER_ROW, input_rows)
        }

        PlanOp::Aggregate { group_by, .. } => {
            let group_factor = if group_by.is_empty() { 1.0 } else { 0.1 };
            let output_rows = (input_rows * group_factor).max(1.0);
            (input_rows * stats::COST_AGGREGATE_PER_ROW, output_rows)
        }

        PlanOp::Project { .. } => (input_rows * stats::COST_PROJECT_PER_ROW, input_rows),

        PlanOp::Sort { .. } => {
            let cost = if input_rows > 1.0 {
                input_rows * input_rows.log2() * stats::COST_SORT_NLOGN
            } else {
                0.0
            };
            (cost, input_rows)
        }

        PlanOp::Limit { count, .. } => {
            let limit_rows = count
                .as_ref()
                .and_then(extract_literal_u64)
                .map(|n| (n as f64).min(input_rows))
                .unwrap_or(input_rows);
            (limit_rows * stats::COST_LIMIT_PER_ROW, limit_rows)
        }

        PlanOp::SetOperation { right, .. } => {
            let right_cost = estimate_cost(&right.ops, stats);
            let right_rows = 1000.0;
            (
                right_cost + input_rows * stats::COST_PROJECT_PER_ROW,
                input_rows + right_rows,
            )
        }

        PlanOp::OptionalMatch { sub_plan } => {
            let sub_cost = estimate_cost(sub_plan, stats);
            // OPTIONAL MATCH preserves all input rows (left-outer-join).
            (
                sub_cost + input_rows * stats::COST_EXPAND_MULTIPLIER * 0.1,
                input_rows,
            )
        }

        PlanOp::IndexIntersection { scans, .. } => {
            let selectivity: f64 = scans
                .iter()
                .map(|_| stats::COST_INDEX_SEEK_FRACTION)
                .product();
            let output_rows = (input_rows * selectivity).max(1.0);
            let cost = input_rows
                * stats::COST_SCAN_PER_ROW
                * stats::COST_INDEX_SEEK_FRACTION
                * stats::COST_INDEX_INTERSECTION_OVERHEAD
                * scans.len() as f64;
            (cost, output_rows)
        }

        PlanOp::WorstCaseOptimalJoin { edges, .. } => {
            let degree = stats.and_then(|s| s.avg_degree()).unwrap_or(10.0);
            let cycle_len = edges.len() as f64;
            let multiplier = degree.powf(cycle_len - 1.0) * stats::COST_WCOJ_FRACTION;
            let output_rows = input_rows * multiplier;
            (
                input_rows * stats::COST_EXPAND_MULTIPLIER * stats::COST_WCOJ_FRACTION * cycle_len,
                output_rows,
            )
        }

        PlanOp::TopK { k, .. } => {
            let k_val = extract_literal_u64(k).unwrap_or(10) as f64;
            let cost = input_rows * k_val.max(1.0).log2() * stats::COST_SORT_NLOGN;
            (cost, k_val.min(input_rows))
        }

        PlanOp::Search { .. } => {
            // Provider-neutral estimate: a vector search is treated as a selective scan
            // until lowering supplies real cardinality.
            let output_rows = input_rows.clamp(1.0, 10.0);
            (
                output_rows * stats::COST_INDEX_SEEK_FRACTION * stats::COST_SCAN_PER_ROW,
                output_rows,
            )
        }

        PlanOp::Materialize { distinct, .. } => {
            let output = if *distinct {
                input_rows * 0.5
            } else {
                input_rows
            };
            (input_rows * stats::COST_MATERIALIZE_PER_ROW, output)
        }

        // ──── DML ────
        PlanOp::InsertVertex { .. } | PlanOp::InsertEdge { .. } => {
            (input_rows * stats::COST_DML_PER_ROW, input_rows)
        }
        PlanOp::SetProperties { .. } | PlanOp::RemoveProperties { .. } => {
            (input_rows * stats::COST_DML_PER_ROW * 0.5, input_rows)
        }
        PlanOp::DeleteVertex { .. } => (input_rows * stats::COST_DML_PER_ROW, 0.0),
        PlanOp::DetachDeleteVertex { .. } => (input_rows * stats::COST_DML_PER_ROW * 2.0, 0.0),
        PlanOp::DeleteEdge { .. } => (input_rows * stats::COST_DML_PER_ROW, 0.0),

        // ──── Procedure / Context ────
        PlanOp::CallProcedure { .. } => (
            stats::COST_PROCEDURE_CALL,
            stats::COST_PROCEDURE_DEFAULT_ROWS,
        ),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            let sub_cost = estimate_cost(&sub_plan.ops, stats);
            let sub_rows = estimate_rows(&sub_plan.ops, stats);
            (
                sub_cost + stats::COST_PROCEDURE_CALL * 0.1,
                sub_rows.max(input_rows),
            )
        }
        PlanOp::UseGraph { sub_plan, .. } => {
            if let Some(sub_ops) = sub_plan {
                let sub_cost = estimate_cost(sub_ops, stats);
                let sub_rows = estimate_rows(sub_ops, stats);
                (stats::COST_USE_GRAPH + sub_cost, sub_rows)
            } else {
                (stats::COST_USE_GRAPH, input_rows)
            }
        }

        // ──── Join ────
        PlanOp::HashJoin { left, right, .. } => {
            let left_cost = estimate_cost(left, stats);
            let right_cost = estimate_cost(right, stats);
            let left_rows = estimate_rows(left, stats);
            let right_rows = estimate_rows(right, stats);
            let build_cost = left_rows.min(right_rows) * stats::COST_HASH_BUILD;
            let probe_cost = left_rows.max(right_rows) * stats::COST_HASH_PROBE;
            let join_rows = (left_rows * right_rows * 0.1).max(1.0);
            (left_cost + right_cost + build_cost + probe_cost, join_rows)
        }
        PlanOp::CartesianProduct { left, right } => {
            let left_cost = estimate_cost(left, stats);
            let right_cost = estimate_cost(right, stats);
            let left_rows = estimate_rows(left, stats);
            let right_rows = estimate_rows(right, stats);
            let product_rows = left_rows * right_rows;
            (
                left_cost + right_cost + product_rows * stats::COST_PROJECT_PER_ROW,
                product_rows,
            )
        }
    }
}

/// Estimate the final row count after all plan operators.
pub fn estimate_rows(ops: &[PlanOp], stats: Option<&dyn GraphStats>) -> f64 {
    let mut rows = estimate_initial_rows(ops, stats);
    for op in ops {
        let (_, new_rows) = estimate_op_cost(op, rows, stats);
        rows = new_rows;
    }
    rows
}

/// Compute the expansion multiplier for variable-length paths.
fn var_len_multiplier(var_len: Option<&VarLenSpec>, degree: f64) -> f64 {
    if let Some(vl) = var_len {
        let max_hops = vl.max.unwrap_or(5) as f64;
        degree.powf(max_hops.min(3.0))
    } else {
        degree
    }
}

/// Estimate the selectivity of a single predicate (0.0 = no rows, 1.0 = all rows).
pub(crate) fn estimate_predicate_selectivity(
    pred: &gleaph_gql::ast::Expr,
    stats: Option<&dyn GraphStats>,
) -> f64 {
    use gleaph_gql::ast::{CmpOp, ExprKind};
    match &pred.kind {
        ExprKind::Compare { left, op, right } => {
            // Check for property access and look up selectivity in stats.
            if let ExprKind::PropertyAccess { property, .. } = &left.kind
                && let Some(stats) = stats
            {
                // Try histogram first.
                if let Some(hist) = stats.property_histogram(property) {
                    if let Some(val) = extract_literal_f64(right) {
                        return hist.range_selectivity(*op, val);
                    }
                    // Histogram exists but no literal → use histogram equality for Eq.
                    if *op == CmpOp::Eq {
                        return hist.equality_selectivity();
                    }
                }
                // Fall back to property_selectivity.
                if let Some(sel) = stats.property_selectivity(property) {
                    return sel;
                }
            }
            match op {
                CmpOp::Eq => 0.1,
                CmpOp::Ne => 0.9,
                _ => 0.3,
            }
        }
        ExprKind::IsNull(_) | ExprKind::IsNotNull(_) => 0.5,
        ExprKind::Not(_) => 0.7,
        ExprKind::And(l, r) => {
            estimate_predicate_selectivity(l, stats) * estimate_predicate_selectivity(r, stats)
        }
        ExprKind::Or(l, r) => {
            let sl = estimate_predicate_selectivity(l, stats);
            let sr = estimate_predicate_selectivity(r, stats);
            (sl + sr - sl * sr).min(1.0)
        }
        _ => 0.3,
    }
}

/// Try to extract a literal f64 from an expression (for histogram lookups).
fn extract_literal_f64(expr: &gleaph_gql::ast::Expr) -> Option<f64> {
    use gleaph_gql::Value;
    if let gleaph_gql::ast::ExprKind::Literal(v) = &expr.kind {
        match v {
            Value::Int32(n) => Some(*n as f64),
            Value::Int64(n) => Some(*n as f64),
            Value::Uint32(n) => Some(*n as f64),
            Value::Uint64(n) => Some(*n as f64),
            Value::Float32(n) => Some(*n as f64),
            Value::Float64(n) => Some(*n),
            _ => None,
        }
    } else {
        None
    }
}

/// Try to extract a literal u64 from an expression (for LIMIT values).
fn extract_literal_u64(expr: &gleaph_gql::ast::Expr) -> Option<u64> {
    use gleaph_gql::Value;
    if let gleaph_gql::ast::ExprKind::Literal(v) = &expr.kind {
        match v {
            Value::Int32(n) => Some(*n as u64),
            Value::Int64(n) => Some(*n as u64),
            Value::Uint32(n) => Some(*n as u64),
            Value::Uint64(n) => Some(*n),
            _ => None,
        }
    } else {
        None
    }
}
