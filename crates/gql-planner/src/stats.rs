//! Graph statistics and cost model constants for the planner.
//!
//! The [`GraphStats`] trait provides optional statistics about the graph that
//! the planner uses for cost-based anchor selection and join ordering.
//! When no statistics are available, the planner falls back to heuristics.

use std::collections::{BTreeMap, BTreeSet};

// ════════════════════════════════════════════════════════════════════════════════
// GraphStats trait
// ════════════════════════════════════════════════════════════════════════════════

/// Statistics about a graph, used by the planner for cost-based decisions.
///
/// All methods have default implementations returning `None`, so consumers
/// can implement only the statistics they have available.
pub trait GraphStats {
    /// Number of vertices with the given node label.
    fn label_cardinality(&self, label: &str) -> Option<u64> {
        self.node_label_cardinality(label)
    }

    /// Number of vertices with the given node label.
    fn node_label_cardinality(&self, label: &str) -> Option<u64> {
        let _ = label;
        None
    }

    /// Number of edges with the given edge label.
    fn edge_label_cardinality(&self, label: &str) -> Option<u64> {
        let _ = label;
        None
    }

    /// Average out-degree of vertices.
    fn avg_degree(&self) -> Option<f64> {
        None
    }

    /// Selectivity of a property (0.0 = no rows, 1.0 = all rows).
    fn property_selectivity(&self, property: &str) -> Option<f64> {
        let _ = property;
        None
    }

    /// Whether a vertex property has an equality index.
    fn is_vertex_property_indexed(&self, property: &str) -> bool {
        let _ = property;
        false
    }

    /// Whether a vertex property has a range index.
    fn is_vertex_property_range_indexed(&self, property: &str) -> bool {
        let _ = property;
        false
    }

    /// Whether an edge property has an index.
    fn is_edge_property_indexed(&self, property: &str) -> bool {
        let _ = property;
        false
    }

    /// Given an edge label, return the possible (source_labels, destination_labels).
    /// Used for schema-aware endpoint inference when a node has no explicit label.
    fn edge_endpoint_labels(&self, edge_label: &str) -> Option<(Vec<String>, Vec<String>)> {
        let _ = edge_label;
        None
    }

    /// Return a histogram for a property, used for range/equality selectivity.
    fn property_histogram(&self, property: &str) -> Option<&PropertyHistogram> {
        let _ = property;
        None
    }
}

pub fn label_cardinality_with_id(stats: &dyn GraphStats, label: &str) -> Option<u64> {
    stats.node_label_cardinality(label)
}

/// An equi-width histogram for property value distribution.
#[derive(Clone, Debug)]
pub struct PropertyHistogram {
    pub min: f64,
    pub max: f64,
    /// Count per bucket (equi-width).
    pub buckets: Vec<u64>,
    pub total: u64,
}

impl PropertyHistogram {
    /// Estimate selectivity for a range predicate.
    pub fn range_selectivity(&self, op: gleaph_gql::ast::CmpOp, value: f64) -> f64 {
        if self.max <= self.min || self.buckets.is_empty() {
            return 0.3; // Fallback.
        }
        let fraction = (value - self.min) / (self.max - self.min);
        let fraction = fraction.clamp(0.0, 1.0);
        match op {
            gleaph_gql::ast::CmpOp::Lt | gleaph_gql::ast::CmpOp::Le => fraction,
            gleaph_gql::ast::CmpOp::Gt | gleaph_gql::ast::CmpOp::Ge => 1.0 - fraction,
            gleaph_gql::ast::CmpOp::Eq => self.equality_selectivity(),
            gleaph_gql::ast::CmpOp::Ne => 1.0 - self.equality_selectivity(),
        }
    }

    /// Estimate selectivity for an equality predicate.
    pub fn equality_selectivity(&self) -> f64 {
        if self.total == 0 || self.buckets.is_empty() {
            return 0.1;
        }
        // Assume uniform distribution within buckets.
        let avg_bucket = self.total as f64 / self.buckets.len() as f64;
        let distinct_estimate =
            self.buckets.iter().filter(|&&b| b > 0).count() as f64 * (avg_bucket.max(1.0));
        (1.0 / distinct_estimate.max(1.0)).min(1.0)
    }
}

/// Concrete statistics struct, compatible with gleaph-old's `TableStats`.
#[derive(Clone, Debug, Default)]
pub struct TableStats {
    pub label_cardinality: BTreeMap<String, u64>,
    pub avg_degree: f64,
    pub property_selectivity: BTreeMap<String, f64>,
    pub indexed_vertex_properties: BTreeSet<String>,
    pub range_indexed_vertex_properties: BTreeSet<String>,
    pub indexed_edge_properties: BTreeSet<String>,
    /// Edge label → (source node labels, destination node labels).
    pub edge_endpoint_labels: BTreeMap<String, (Vec<String>, Vec<String>)>,
    /// Property histograms for selectivity estimation.
    pub property_histograms: BTreeMap<String, PropertyHistogram>,
}

impl GraphStats for TableStats {
    fn label_cardinality(&self, label: &str) -> Option<u64> {
        self.label_cardinality.get(label).copied()
    }

    fn node_label_cardinality(&self, label: &str) -> Option<u64> {
        self.label_cardinality.get(label).copied()
    }

    fn avg_degree(&self) -> Option<f64> {
        if self.avg_degree > 0.0 {
            Some(self.avg_degree)
        } else {
            None
        }
    }

    fn property_selectivity(&self, property: &str) -> Option<f64> {
        self.property_selectivity.get(property).copied()
    }

    fn is_vertex_property_indexed(&self, property: &str) -> bool {
        self.indexed_vertex_properties.contains(property)
    }

    fn is_vertex_property_range_indexed(&self, property: &str) -> bool {
        self.range_indexed_vertex_properties.contains(property)
    }

    fn is_edge_property_indexed(&self, property: &str) -> bool {
        self.indexed_edge_properties.contains(property)
    }

    fn edge_endpoint_labels(&self, edge_label: &str) -> Option<(Vec<String>, Vec<String>)> {
        self.edge_endpoint_labels.get(edge_label).cloned()
    }

    fn property_histogram(&self, property: &str) -> Option<&PropertyHistogram> {
        self.property_histograms.get(property)
    }
}

/// No statistics available — planner uses pure heuristics.
pub struct NoStats;

impl GraphStats for NoStats {}

// ════════════════════════════════════════════════════════════════════════════════
// Cost model constants (ported from gleaph-old)
// ════════════════════════════════════════════════════════════════════════════════

/// Cost of scanning a single row in a full/label scan.
pub const COST_SCAN_PER_ROW: f64 = 100.0;

/// Fraction of cost for an index seek relative to a full scan.
/// Index seeks are ~100x faster.
pub const COST_INDEX_SEEK_FRACTION: f64 = 0.01;

/// Cost of evaluating a WHERE filter per row.
pub const COST_FILTER_PER_ROW: f64 = 50.0;

/// Cost multiplier for edge expansion per intermediate row.
pub const COST_EXPAND_MULTIPLIER: f64 = 1000.0;

/// Cost of BFS / shortest-path per source row.
pub const COST_SHORTEST_PER_ROW: f64 = 5000.0;

/// Cost of aggregation per row.
pub const COST_AGGREGATE_PER_ROW: f64 = 200.0;

/// Cost of projection per row.
pub const COST_PROJECT_PER_ROW: f64 = 10.0;

/// Cost of limit truncation per row.
pub const COST_LIMIT_PER_ROW: f64 = 1.0;

/// Sort cost coefficient (for N * log(N) model).
pub const COST_SORT_NLOGN: f64 = 50.0;

/// Cost of a DML operation (INSERT/DELETE) per row.
pub const COST_DML_PER_ROW: f64 = 500.0;

/// Cost of materializing intermediate results per row.
pub const COST_MATERIALIZE_PER_ROW: f64 = 20.0;

/// WCOJ cost fraction relative to pairwise joins (cheaper for cyclic patterns).
pub const COST_WCOJ_FRACTION: f64 = 0.3;

/// Overhead multiplier for index intersection (multiple index lookups + merge).
pub const COST_INDEX_INTERSECTION_OVERHEAD: f64 = 1.2;

/// Cost of a procedure call (opaque, constant estimate).
pub const COST_PROCEDURE_CALL: f64 = 2000.0;
/// Default estimated rows returned by a procedure call.
pub const COST_PROCEDURE_DEFAULT_ROWS: f64 = 100.0;
/// Cost of a graph context switch (USE GRAPH).
pub const COST_USE_GRAPH: f64 = 500.0;
/// Cost of building the hash table (per build-side row).
pub const COST_HASH_BUILD: f64 = 150.0;
/// Cost of probing the hash table (per probe-side row).
pub const COST_HASH_PROBE: f64 = 50.0;
