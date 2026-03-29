use crate::ast::AggFunc;
use crate::ast::QueryStmt;
use std::fmt::Write;

/// A logical operator in the physical execution plan.
///
/// Operators are listed in execution order inside [`PhysicalPlan::ops`].
#[derive(Clone, Debug, PartialEq)]
pub enum PlanOp {
    /// Property equality index lookup to seed rows (requires secondary index support).
    IndexScan,
    /// Edge property equality index lookup to seed (src, dst) pairs.
    EdgeIndexScan,
    /// Full vertex scan (or label-filtered scan) to seed the pipeline.
    NodeScan,
    /// Branch between index scan and node scan based on a parameter value at runtime.
    /// Emitted when a `$param IS NULL OR var.prop = $param` pattern is detected
    /// and the property has an equality index.
    ConditionalIndexScan,
    /// Apply WHERE predicate filters on intermediate rows.
    PropertyFilter,
    /// Traverse outgoing edges to reach neighbouring nodes.
    Expand,
    /// Apply edge inline-property hints as an inline filter immediately after an Expand step.
    FilterEdge,
    /// Execute shortest-path expansion for a `MATCH SHORTEST ...` pattern.
    ShortestPath,
    /// Perform grouping/aggregation before projection.
    Aggregate,
    /// Evaluate RETURN expressions and emit output columns.
    Project,
    /// Sort all rows according to an ORDER BY clause.
    Sort,
    /// Truncate the result set to at most `n` rows.
    Limit,
}

/// Metadata attached to a [`PhysicalPlan`] by the planner.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanAnnotations {
    /// The variable chosen as the starting point for the node scan.
    pub chosen_anchor: Option<String>,
    /// Human-readable description of why the anchor was chosen
    /// (e.g. `"property-equality"`, `"label-only"`, `"full-scan"`).
    pub estimated_cardinality_source: Option<String>,
    /// Structured reason mirroring `estimated_cardinality_source`.
    pub estimated_cardinality_reason: Option<PlanCardinalityReason>,
    /// Planner-estimated row count for the plan result (coarse).
    pub estimated_rows: Option<f64>,
    /// Planner-estimated instruction cost for the plan (coarse).
    pub estimated_instructions: Option<f64>,
    /// Greedy left-deep expansion order for MATCH chains (0-based chain indices).
    pub join_order: Option<Vec<usize>>,
    /// Reordered execution order for multiple MATCH clauses (0-based clause indices).
    /// Index 0 is always the first clause (anchor); remaining indices are reordered
    /// by selectivity. `None` when only one MATCH clause.
    pub match_clause_order: Option<Vec<usize>>,
    /// Earliest stage index at which conjunctive WHERE predicates were placed.
    /// `0` means immediately after `NodeScan`, `1` after first expansion, etc.
    pub filter_pushdown_stages: Option<Vec<usize>>,
    /// Whether LIMIT was pushed before projection because ORDER BY/aggregation was absent.
    pub limit_pushdown_applied: bool,
    /// For SHORTEST single-chain outgoing patterns: the end-node variable name when
    /// the planner estimates it has lower cardinality than the start-node, suggesting
    /// reverse-anchor BFS (iterate target candidates, BFS backward to start candidates).
    pub shortest_reverse_anchor: Option<String>,
    /// Metadata for conditional index scan. Used with `PlanOp::ConditionalIndexScan`.
    pub conditional_scan: Option<ConditionalScanInfo>,
    /// Comparison operator for `PlanOp::IndexScan`. `None` means equality (default).
    /// Set to `Some(Ge/Gt/Le/Lt)` when a range index scan is used.
    pub index_scan_cmp_op: Option<ConditionalCmpOp>,
    /// Property names referenced by semantic property-access constraints.
    pub semantic_property_accesses: Option<Vec<String>>,
    /// Property names referenced specifically inside boolean contexts such as WHERE/HAVING.
    pub semantic_where_property_accesses: Option<Vec<String>>,
    /// WHERE-side vertex properties that have an equality index according to planner stats.
    pub semantic_indexable_vertex_properties: Option<Vec<String>>,
    /// WHERE-side vertex properties that have a range index according to planner stats.
    pub semantic_range_indexable_vertex_properties: Option<Vec<String>>,
    /// WHERE-side edge properties that have an equality index according to planner stats.
    pub semantic_indexable_edge_properties: Option<Vec<String>>,
    /// Human-readable summary of semantic facts relevant to scan/index choice.
    pub semantic_scan_reason: Option<String>,
    /// Property names from semantic facts that fed conditional scan candidate selection.
    pub semantic_conditional_scan_properties: Option<Vec<String>>,
    /// Aggregate functions observed by semantic analysis.
    pub semantic_aggregates: Option<Vec<AggFunc>>,
    /// Flow-sensitive narrowing facts extracted from WHERE predicates.
    pub narrowing_facts: Option<Vec<crate::semantic::NarrowingFact>>,
    /// Type diagnostics produced by constraint-based type checking during planning.
    /// `None` when no schema was provided to the planner.
    pub type_diagnostics: Option<Vec<crate::type_check::TypeWarning>>,
    /// Whether the query is statically contradictory (has an `ImpossiblePattern` warning).
    /// When `true`, the executor can skip execution and return an empty result.
    pub statically_contradictory: bool,
}

impl PlanAnnotations {
    /// Returns a stable, human-readable summary of planner annotations.
    ///
    /// Intended for explain/debug surfaces that want readable output without
    /// parsing the derived `Debug` representation.
    pub fn explain_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(anchor) = &self.chosen_anchor {
            lines.push(format!("anchor={anchor}"));
        }
        if let Some(source) = &self.estimated_cardinality_source {
            lines.push(format!("estimated-cardinality-source={source}"));
        }
        if let Some(reason) = &self.estimated_cardinality_reason {
            lines.push(format!("estimated-cardinality-reason={}", reason.explain()));
        }
        if let Some(reason) = &self.semantic_scan_reason {
            lines.push(format!("semantic-scan-reason={reason}"));
        }
        if let Some(props) = &self.semantic_where_property_accesses {
            lines.push(format!(
                "semantic-where-properties={}",
                comma_join(props.iter().map(String::as_str))
            ));
        }
        if let Some(props) = &self.semantic_indexable_vertex_properties {
            lines.push(format!(
                "semantic-indexable-vertex-properties={}",
                comma_join(props.iter().map(String::as_str))
            ));
        }
        if let Some(props) = &self.semantic_range_indexable_vertex_properties {
            lines.push(format!(
                "semantic-range-indexable-vertex-properties={}",
                comma_join(props.iter().map(String::as_str))
            ));
        }
        if let Some(props) = &self.semantic_indexable_edge_properties {
            lines.push(format!(
                "semantic-indexable-edge-properties={}",
                comma_join(props.iter().map(String::as_str))
            ));
        }
        if let Some(cond) = &self.conditional_scan {
            lines.extend(cond.explain_lines());
        }
        if let Some(aggs) = &self.semantic_aggregates {
            lines.push(format!(
                "semantic-aggregates={}",
                comma_join(aggs.iter().map(agg_func_name))
            ));
        }
        if let Some(facts) = &self.narrowing_facts
            && !facts.is_empty()
        {
            let summary: Vec<String> = facts
                .iter()
                .map(|f| match f {
                    crate::semantic::NarrowingFact::PropertyNonNull { var, property } => {
                        format!("{var}.{property}:nonnull")
                    }
                    crate::semantic::NarrowingFact::LabelNarrowed { var, label } => {
                        format!("{var}:label({label})")
                    }
                    crate::semantic::NarrowingFact::EdgeLabelNarrowed { var, label } => {
                        format!("{var}:edge-label({label})")
                    }
                })
                .collect();
            lines.push(format!("semantic-narrowing={}", summary.join(",")));
        }
        if self.statically_contradictory {
            lines.push("statically-contradictory=true".to_string());
        }
        if let Some(diags) = &self.type_diagnostics
            && !diags.is_empty()
        {
            lines.push(format!("type-diagnostic-count={}", diags.len()));
        }
        lines
    }
}

/// Structured planner explanation for the primary scan/cardinality decision.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanCardinalityReason {
    PropertyIndex {
        property: String,
        comparison: Option<ConditionalCmpOp>,
    },
    EdgePropertyIndex {
        property: String,
    },
    InlinePropertyIndex {
        property: String,
    },
    InlineWhereIndex {
        property: String,
    },
    AnchorHeuristic {
        kind: String,
    },
}

impl PlanCardinalityReason {
    pub fn explain(&self) -> String {
        match self {
            Self::PropertyIndex {
                property,
                comparison,
            } => match comparison {
                Some(cmp) => format!("property-index({property}, {})", cmp.as_str()),
                None => format!("property-index({property}, eq)"),
            },
            Self::EdgePropertyIndex { property } => {
                format!("edge-property-index({property})")
            }
            Self::InlinePropertyIndex { property } => {
                format!("inline-property-index({property})")
            }
            Self::InlineWhereIndex { property } => {
                format!("inline-where-index({property})")
            }
            Self::AnchorHeuristic { kind } => format!("anchor-heuristic({kind})"),
        }
    }
}

/// Information needed to execute a conditional index scan at runtime.
///
/// Contains one or more candidates. At execution time, the executor picks the
/// first candidate whose parameter is non-NULL and performs an index lookup;
/// if all parameters are NULL, it falls back to a full/label scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ConditionalScanInfo {
    /// Candidates ordered by planner preference (best selectivity first).
    pub candidates: Vec<ConditionalScanCandidate>,
    /// Structured explanation for candidate ordering and selection inputs.
    pub reasoning: Option<ConditionalScanReasoning>,
}

/// Comparison operator for a conditional scan candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConditionalCmpOp {
    /// Equality: `var.prop = $param`
    Eq,
    /// Greater-than-or-equal: `var.prop >= $param`
    Ge,
    /// Greater-than: `var.prop > $param`
    Gt,
    /// Less-than-or-equal: `var.prop <= $param`
    Le,
    /// Less-than: `var.prop < $param`
    Lt,
}

impl ConditionalCmpOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ge => "ge",
            Self::Gt => "gt",
            Self::Le => "le",
            Self::Lt => "lt",
        }
    }
}

/// A single optional-filter candidate for conditional index scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ConditionalScanCandidate {
    /// Parameter name to check for NULL.
    pub param_name: String,
    /// Property name to use for index lookup.
    pub property: String,
    /// Variable name that owns the property.
    pub variable: String,
    /// Comparison operator (Eq for equality, Ge/Gt/Le/Lt for range).
    pub cmp_op: ConditionalCmpOp,
}

/// Structured explanation for `ConditionalIndexScan` planning.
#[derive(Clone, Debug, PartialEq)]
pub struct ConditionalScanReasoning {
    /// Properties from semantic boolean-context analysis that overlapped with conditional candidates.
    pub semantic_properties: Vec<String>,
    /// Ordered candidate explanations in planner preference order.
    pub candidate_reasons: Vec<ConditionalScanCandidateReason>,
}

/// Explanation for a single conditional scan candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct ConditionalScanCandidateReason {
    pub property: String,
    pub variable: String,
    pub cmp_op: ConditionalCmpOp,
    pub selectivity_hint: Option<f64>,
}

impl ConditionalScanInfo {
    pub fn explain_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let mut candidate_summary = String::new();
        for (idx, candidate) in self.candidates.iter().enumerate() {
            if idx > 0 {
                candidate_summary.push_str(", ");
            }
            let _ = write!(
                candidate_summary,
                "{}.{}:{}:${}",
                candidate.variable,
                candidate.property,
                candidate.cmp_op.as_str(),
                candidate.param_name
            );
        }
        lines.push(format!("conditional-scan-candidates={candidate_summary}"));
        if let Some(reasoning) = &self.reasoning {
            if !reasoning.semantic_properties.is_empty() {
                lines.push(format!(
                    "conditional-scan-semantic-properties={}",
                    comma_join(reasoning.semantic_properties.iter().map(String::as_str))
                ));
            }
            for reason in &reasoning.candidate_reasons {
                match reason.selectivity_hint {
                    Some(selectivity) => lines.push(format!(
                        "conditional-scan-candidate={}.{}:{} (selectivity={selectivity:.3})",
                        reason.variable,
                        reason.property,
                        reason.cmp_op.as_str()
                    )),
                    None => lines.push(format!(
                        "conditional-scan-candidate={}.{}:{}",
                        reason.variable,
                        reason.property,
                        reason.cmp_op.as_str()
                    )),
                }
            }
        }
        lines
    }
}

/// A physical execution plan ready to be handed to [`crate::executor`].
///
/// Produced by [`crate::planner::build_plan`] from a validated [`Statement`].
/// Currently only query statements are supported; mutations bypass the planner.
#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalPlan {
    /// Ordered sequence of logical operators to execute.
    pub ops: Vec<PlanOp>,
    /// Planner annotations (anchor choice, cardinality hints).
    pub annotations: PlanAnnotations,
    /// The original query statement, carried along so the executor can access
    /// clauses (WHERE, RETURN, ORDER BY, LIMIT) during evaluation.
    pub query: Option<QueryStmt>,
}

impl PhysicalPlan {
    /// Returns a stable human-readable plan summary suitable for explain/debug output.
    pub fn explain_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "ops={}",
            comma_join(self.ops.iter().map(PlanOp::as_str))
        )];
        lines.extend(self.annotations.explain_lines());
        lines
    }
}

impl PlanOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IndexScan => "IndexScan",
            Self::EdgeIndexScan => "EdgeIndexScan",
            Self::NodeScan => "NodeScan",
            Self::ConditionalIndexScan => "ConditionalIndexScan",
            Self::PropertyFilter => "PropertyFilter",
            Self::Expand => "Expand",
            Self::FilterEdge => "FilterEdge",
            Self::ShortestPath => "ShortestPath",
            Self::Aggregate => "Aggregate",
            Self::Project => "Project",
            Self::Sort => "Sort",
            Self::Limit => "Limit",
        }
    }
}

fn comma_join<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values.into_iter().collect::<Vec<_>>().join(",")
}

fn agg_func_name(func: &AggFunc) -> &'static str {
    match func {
        AggFunc::Count => "count",
        AggFunc::Sum => "sum",
        AggFunc::Avg => "avg",
        AggFunc::Min => "min",
        AggFunc::Max => "max",
        AggFunc::Collect => "collect",
        AggFunc::StringAgg => "string_agg",
        AggFunc::PercentileCont => "percentile_cont",
        AggFunc::PercentileDisc => "percentile_disc",
    }
}
