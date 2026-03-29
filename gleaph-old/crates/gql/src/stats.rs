use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Cost-model constants (calibrated from canbench IC instruction measurements)
//
// All constants are normalised relative to COST_SCAN_PER_ROW = 1.0.
// One "unit" ≈ 5,014 IC instructions (measured: bench_gql_full_scan_100,
// 100 labeled vertices with one property access → 501,420 IC / 100 rows).
// ---------------------------------------------------------------------------

/// Per-row baseline cost for a full vertex scan (reference unit = 1.0).
pub const COST_SCAN_PER_ROW: f64 = 1.0;

/// Per-row cost for a secondary-index seek (variable-cost component).
///
/// Formula in estimate_cost: `(matching_rows + 10) × COST_INDEX_SEEK_FRACTION`.
/// Calibrated from bench_gql_index_seek_1_of_100: 219,160 IC for 1 matching row
/// out of 100 indexed vertices (default selectivity 0.1 → 10 estimated matches).
/// (100×0.1 + 10) × c + project = 219,160 / 5,014 = 43.7 units → c ≈ 2.18.
pub const COST_INDEX_SEEK_FRACTION: f64 = 2.18;

/// Per-degree multiplier for an Expand step.
/// Formula: `instr += rows × avg_degree × COST_EXPAND_MULTIPLIER`.
/// Calibrated from bench_gql_expand_degree_10: 315,820 IC for 1 source vertex + 10 edges,
/// excluding ~1-vertex scan overhead ≈ 5K → (315,820 − 5,014) / 10 / 5,014 ≈ 6.19.
pub const COST_EXPAND_MULTIPLIER: f64 = 6.19;

/// Per-row multiplier for evaluating a scalar property filter predicate.
/// Calibrated from bench_gql_property_filter_100 vs bench_gql_full_scan_100:
/// extra (705,710 − 501,420) = 204,290 IC / 100 rows → 2,043 / 5,014 = 0.407.
pub const COST_FILTER_PER_ROW: f64 = 0.407;

/// Per-row multiplier for a GROUP BY / aggregation step.
/// Calibrated from bench_gql_aggregate_50: 555,550 IC total; scan-50 baseline ≈ 250,700 IC.
/// Extra (555,550 − 250,700) / 50 / 5,014 ≈ 1.22.
pub const COST_AGGREGATE_PER_ROW: f64 = 1.22;

/// Coefficient for the n·log₂(n) sort cost formula.
/// Calibrated from bench_gql_sort_50: 413,340 IC; scan-50 baseline ≈ 250,700 IC.
/// Extra 162,640 IC / 5,014 = 32.4 units; 50 × log₂(50) = 282.2 → c = 32.4 / 282.2 = 0.115.
pub const COST_SORT_NLOGN: f64 = 0.115;

/// Per-row cost for LIMIT evaluation (bookkeeping overhead).
///
/// Calibrated from bench_gql_limit_100 vs bench_gql_full_scan_100:
/// extra (512,690 − 501,420) = 11,270 IC / 100 rows = 113 IC/row → 113 / 5,014 = 0.022.
pub const COST_LIMIT_PER_ROW: f64 = 0.022;

/// Per-row cost for RETURN projection (incremental property-access overhead).
///
/// Calibrated from bench_gql_full_scan_100 (RETURN n.id) vs bench_gql_project_constant_100
/// (RETURN 1): extra (501,420 − 457,030) = 44,390 IC / 100 rows / 5,014 = 0.089.
/// The base projection cost is embedded in COST_SCAN_PER_ROW (baseline includes RETURN).
pub const COST_PROJECT_PER_ROW: f64 = 0.089;

/// Per-row cost for a SHORTEST path expansion (BFS).
///
/// Calibrated from bench_gql_shortest_path_chain: 6-node linear chain
/// (Start→Step×4→End), 1 source vertex, BFS depth 5 → 87,440 IC total.
/// Normalized: 87,440 / 5,014 = 17.44 units.
/// Subtract NodeScan (1.0) and Project (0.089): 17.44 − 1.089 = 16.35.
pub const COST_SHORTEST_PER_ROW: f64 = 16.35;

/// Planner-oriented graph statistics used for cost estimation and heuristics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableStats {
    /// Approximate/known number of active vertices for each label.
    pub label_cardinality: BTreeMap<String, u64>,
    /// Average out-degree across active vertices.
    pub avg_degree: f64,
    /// Property selectivity estimates keyed by `entity:prop` (e.g. `vertex:id`).
    pub property_selectivity: BTreeMap<String, f64>,
    /// Registered secondary equality indexes for vertex properties.
    pub indexed_vertex_properties: BTreeSet<String>,
    /// Registered secondary range indexes for vertex properties.
    pub range_indexed_vertex_properties: BTreeSet<String>,
    /// Registered secondary equality indexes for edge properties.
    pub indexed_edge_properties: BTreeSet<String>,
    /// Optional total counts to support coarse row estimation.
    pub vertex_count: u64,
    pub edge_count: u64,
}

/// Cost estimate attached to a physical operator or partial plan.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CostEstimate {
    pub estimated_rows: f64,
    pub estimated_instructions: f64,
}
