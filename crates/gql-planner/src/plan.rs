//! Physical plan types for the GQL planner.
//!
//! A [`PhysicalPlan`] represents a sequence of [`PlanOp`] operators that an
//! executor can walk to evaluate a GQL query. The planner produces these from
//! the parsed AST; the executor (in a separate crate) consumes them.
//!
//! ## Property projection and DML
//!
//! Expand / scan operators may carry `property_projection` lists so the executor hydrates only the
//! properties needed by `RETURN` and downstream expressions; see
//! [`crate::property_projection::apply_node_property_projections`].
//!
//! [`PhysicalPlan::has_dml`] is true when any operator in the plan (including nested sub-plans under
//! joins, `OPTIONAL MATCH`, `USE GRAPH`, etc.) is graph-mutating. Executors use it to decide whether a
//! terminal `GraphWrite::flush` is needed after the plan.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;

pub use gleaph_gql::ast::CmpOp;
use gleaph_gql::ast::{AggregateFunc, Expr, ExprKind, LetBinding, OrderByClause};
pub use gleaph_gql::type_check::DmlDiagnostic as PlannerDiagnostic;
pub use gleaph_gql::type_check::TypeDiagnostic;
use gleaph_gql::types::{EdgeDirection, LabelExpr};

/// Cheaply-cloneable string type for identifiers (variable names, labels, properties, etc.).
pub type Str = Rc<str>;

/// A node-label reference in a physical plan.
///
/// This is intentionally a planner-generic name wrapper, not a backend label id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeLabelRef {
    pub name: Str,
}

impl NodeLabelRef {
    pub fn new(name: impl Into<Str>) -> Self {
        Self { name: name.into() }
    }
}

impl AsRef<str> for NodeLabelRef {
    fn as_ref(&self) -> &str {
        &self.name
    }
}

impl Deref for NodeLabelRef {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.name
    }
}

impl fmt::Display for NodeLabelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(f)
    }
}

impl From<&str> for NodeLabelRef {
    fn from(value: &str) -> Self {
        Self::new(Str::from(value))
    }
}

impl From<String> for NodeLabelRef {
    fn from(value: String) -> Self {
        Self::new(Str::from(value))
    }
}

impl From<Str> for NodeLabelRef {
    fn from(value: Str) -> Self {
        Self::new(value)
    }
}

/// An edge-label reference in a physical plan.
///
/// This is intentionally a planner-generic name wrapper, not a backend label id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeLabelRef {
    pub name: Str,
}

impl EdgeLabelRef {
    pub fn new(name: impl Into<Str>) -> Self {
        Self { name: name.into() }
    }
}

impl AsRef<str> for EdgeLabelRef {
    fn as_ref(&self) -> &str {
        &self.name
    }
}

impl Deref for EdgeLabelRef {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.name
    }
}

impl fmt::Display for EdgeLabelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(f)
    }
}

impl From<&str> for EdgeLabelRef {
    fn from(value: &str) -> Self {
        Self::new(Str::from(value))
    }
}

impl From<String> for EdgeLabelRef {
    fn from(value: String) -> Self {
        Self::new(Str::from(value))
    }
}

impl From<Str> for EdgeLabelRef {
    fn from(value: Str) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LabelUseIntent {
    ReadExisting,
    CreateIfMissing,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanLabelUses {
    pub node_labels: BTreeMap<Str, LabelUseIntent>,
    pub edge_labels: BTreeMap<Str, LabelUseIntent>,
}

impl PlanLabelUses {
    fn add_node(&mut self, label: &NodeLabelRef, intent: LabelUseIntent) {
        merge_label_intent(&mut self.node_labels, label.name.clone(), intent);
    }

    fn add_edge(&mut self, label: &EdgeLabelRef, intent: LabelUseIntent) {
        merge_label_intent(&mut self.edge_labels, label.name.clone(), intent);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PropertyUseIntent {
    ReadExisting,
    CreateIfMissing,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanPropertyUses {
    pub properties: BTreeMap<Str, PropertyUseIntent>,
}

impl PlanPropertyUses {
    fn add_property(&mut self, name: &Str, intent: PropertyUseIntent) {
        merge_property_intent(&mut self.properties, name.clone(), intent);
    }
}

fn merge_property_intent(
    properties: &mut BTreeMap<Str, PropertyUseIntent>,
    name: Str,
    intent: PropertyUseIntent,
) {
    properties
        .entry(name)
        .and_modify(|existing| {
            if intent == PropertyUseIntent::CreateIfMissing {
                *existing = intent;
            }
        })
        .or_insert(intent);
}

fn merge_label_intent(
    labels: &mut BTreeMap<Str, LabelUseIntent>,
    name: Str,
    intent: LabelUseIntent,
) {
    labels
        .entry(name)
        .and_modify(|existing| {
            if intent == LabelUseIntent::CreateIfMissing {
                *existing = intent;
            }
        })
        .or_insert(intent);
}

/// Variable import scope for an inline procedure call.
#[derive(Clone, Debug)]
pub enum InlineProcedureScope {
    /// No scope clause was written; the full outer scope is visible.
    ImplicitAll,
    /// A scope clause was written; only these variables are visible.
    Explicit(Vec<Str>),
}

impl InlineProcedureScope {
    /// Returns the explicitly imported variables, if a scope clause was written.
    pub fn explicit_vars(&self) -> Option<&[Str]> {
        match self {
            Self::ImplicitAll => None,
            Self::Explicit(vars) => Some(vars),
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// PhysicalPlan
// ════════════════════════════════════════════════════════════════════════════════

/// A physical query plan: an ordered sequence of operators plus annotations.
#[derive(Clone, Debug, Default)]
pub struct PhysicalPlan {
    /// Operators in execution order.
    pub ops: Vec<PlanOp>,
    /// Language/type diagnostics associated with this plan.
    pub diagnostics: PlanDiagnostics,
    /// Metadata produced during planning (cost estimates, anchor info, etc.).
    pub annotations: PlanAnnotations,
    /// Final RETURN / WITH column layout for result hydration.
    pub output: crate::output_schema::OutputSchema,
    /// Dense variable slots for executor row storage.
    pub binding_layout: crate::binding_layout::BindingLayout,
}

impl PhysicalPlan {
    /// Build a plan and derive [`crate::output_schema::OutputSchema`] from its ops.
    pub fn from_ops(mut ops: Vec<PlanOp>) -> Self {
        let mut annotations = PlanAnnotations::default();
        crate::pushdown::apply_shortest_path_binding_pruning(&mut ops, &mut annotations);
        let output = crate::output_schema::derive_output_schema(&ops);
        let binding_layout = crate::binding_layout::derive_binding_layout(&ops);
        Self {
            ops,
            output,
            annotations,
            binding_layout,
            ..Self::default()
        }
    }

    /// True if any operator in this plan (including nested sub-plans) is DML.
    pub fn has_dml(&self) -> bool {
        ops_contain_dml(&self.ops)
    }

    /// True iff this plan creates only brand-new elements: it has at least one `INSERT` and no
    /// operator that reads or binds existing graph state. Such a plan needs no index anchor and
    /// no seeds — every edge endpoint is a freshly inserted vertex — so a router may place the
    /// whole plan on a single shard and execute it there atomically. Returns false for any
    /// read/scan/traversal operator, for non-insert DML (`SET`/`REMOVE`/`DELETE`), and for a plan
    /// with no DML at all.
    pub fn is_pure_insert(&self) -> bool {
        ops_are_pure_insert(&self.ops)
    }

    /// True iff this plan is a *single-anchor threaded bundle*: it reads existing graph state in
    /// exactly one place — a single leading index/label anchor a router can resolve to a shard set
    /// by index lookup — and every later operator only mutates threaded bindings, inserts new
    /// elements, or reshapes already-bound rows (no second scan, traversal, join, or sub-plan that
    /// reaches back into the graph). Such a bundle performs no cross-shard reads, so when its
    /// anchor resolves to a single shard the whole (possibly multi-statement) program can run there
    /// atomically (ADR 0029 Phase 5, contract 1, anchored single-shard). Returns false for pure
    /// reads (no DML), anchorless plans, and any plan with a non-leading existing-state read.
    pub fn is_single_anchor_threaded_bundle(&self) -> bool {
        match self.ops.split_first() {
            Some((first, rest)) => {
                op_is_seedable_anchor(first)
                    && self.has_dml()
                    && !rest.iter().any(op_reads_existing_graph)
            }
            None => false,
        }
    }

    pub fn use_graph_pushdown(&self) -> &[UseGraphPushdownInfo] {
        &self.annotations.optimizer.use_graph_pushdown
    }

    /// Collect node-label and edge-label names referenced by this plan.
    pub fn label_uses(&self) -> PlanLabelUses {
        let mut uses = PlanLabelUses::default();
        collect_label_uses_in_ops(&self.ops, &mut uses);
        uses
    }

    /// Collect property names referenced by this plan.
    pub fn property_uses(&self) -> PlanPropertyUses {
        let mut uses = PlanPropertyUses::default();
        collect_property_uses_in_ops(&self.ops, &mut uses);
        uses
    }
}

#[derive(Clone, Debug, Default)]
pub struct PlanDiagnostics {
    /// Fatal DML issues that make execution invalid.
    pub dml_errors: Vec<PlannerDiagnostic>,
    /// Non-fatal DML issues.
    pub dml_warnings: Vec<PlannerDiagnostic>,
    /// Non-DML language diagnostics from type checking.
    pub type_warnings: Vec<TypeDiagnostic>,
}

#[derive(Clone, Debug, Default)]
pub struct PlanSummary {
    pub estimated_rows: Option<f64>,
    pub estimated_cost: Option<f64>,
    pub has_dml: bool,
    pub dml_error_count: usize,
    pub dml_warning_count: usize,
    pub type_warning_count: usize,
}

impl PlanSummary {
    pub fn from_plan(plan: &PhysicalPlan) -> Self {
        Self {
            estimated_rows: plan.annotations.optimizer.estimated_rows,
            estimated_cost: plan.annotations.optimizer.estimated_cost,
            has_dml: plan.has_dml(),
            dml_error_count: plan.diagnostics.dml_errors.len(),
            dml_warning_count: plan.diagnostics.dml_warnings.len(),
            type_warning_count: plan.diagnostics.type_warnings.len(),
        }
    }
}

/// Returns `true` if the plan (including nested sub-plans) contains any `PlanOp::Search`.
pub fn plan_contains_search(plan: &PhysicalPlan) -> bool {
    ops_contain_search(&plan.ops)
}

fn ops_contain_search(ops: &[PlanOp]) -> bool {
    ops.iter().any(|op| match op {
        PlanOp::Search { .. } => true,
        PlanOp::HashJoin { left, right, .. } => {
            ops_contain_search(left) || ops_contain_search(right)
        }
        PlanOp::CartesianProduct { left, right } => {
            ops_contain_search(left) || ops_contain_search(right)
        }
        PlanOp::SetOperation { right, .. } => ops_contain_search(&right.ops),
        PlanOp::OptionalMatch { sub_plan } => ops_contain_search(sub_plan),
        PlanOp::InlineProcedureCall { sub_plan, .. } => ops_contain_search(&sub_plan.ops),
        PlanOp::UseGraph {
            sub_plan: Some(sp), ..
        } => ops_contain_search(sp),
        _ => false,
    })
}

// ════════════════════════════════════════════════════════════════════════════════
// PlanOp
// ════════════════════════════════════════════════════════════════════════════════

/// A single operator in the physical plan.
#[derive(Clone, Debug)]
pub enum PlanOp {
    // ──── Scan ────
    /// Full or label-filtered vertex scan.
    NodeScan {
        /// Variable bound to each scanned vertex.
        variable: Str,
        /// Optional label constraint (only vertices with this label).
        label: Option<NodeLabelRef>,
        /// When set, only these property keys are hydrated on each bound vertex record.
        /// `None` retains full property maps (legacy path).
        property_projection: Option<Rc<[Str]>>,
    },

    /// Equality or range scan on an indexed vertex property.
    IndexScan {
        variable: Str,
        property: Str,
        value: ScanValue,
        cmp: CmpOp,
        property_projection: Option<Rc<[Str]>>,
    },

    /// Equality scan on an indexed edge property.
    EdgeIndexScan {
        variable: Str,
        property: Str,
        value: ScanValue,
        property_projection: Option<Rc<[Str]>>,
    },

    /// After [`PlanOp::EdgeIndexScan`], bind the path endpoint nodes from the edge
    /// record (`src` / `dst`) consistent with pattern [`EdgeDirection`] (same as [`PlanOp::Expand`]).
    EdgeBindEndpoints {
        edge: Str,
        near: Str,
        far: Str,
        direction: EdgeDirection,
        /// When set, rows whose edge label does not match are dropped.
        label: Option<EdgeLabelRef>,
        near_property_projection: Option<Rc<[Str]>>,
        far_property_projection: Option<Rc<[Str]>>,
        /// When set, executor binds hop auxiliary bytes under this name (same semantics as [`PlanOp::Expand::hop_aux_binding`]).
        hop_aux_binding: Option<Str>,
    },

    /// Parameter-based conditional scan: tries index scan if parameter is
    /// non-null, falls back to label/full scan otherwise.
    ConditionalIndexScan {
        candidates: Vec<ConditionalScanCandidate>,
        /// Fallback label for full scan when all parameters are null.
        fallback_label: Option<NodeLabelRef>,
        fallback_variable: Str,
        property_projection: Option<Rc<[Str]>>,
    },

    // ──── Filter ────
    /// Apply one or more predicates (from WHERE or inline pattern constraints).
    PropertyFilter {
        /// The conjunctive predicates to evaluate.
        predicates: Vec<Expr>,
        /// Pipeline stage at which this filter is applied (0 = after initial scan).
        stage: usize,
    },

    // ──── Traversal ────
    /// Expand along edges from a source vertex.
    Expand {
        src: Str,
        edge: Str,
        dst: Str,
        direction: EdgeDirection,
        label: Option<EdgeLabelRef>,
        /// General edge label predicate (disjunction, negation, `&`, …). When set, `label` is `None`.
        label_expr: Option<LabelExpr>,
        /// Variable-length expansion bounds (e.g. `*2..5`).
        var_len: Option<VarLenSpec>,
        /// When set, expand by probing an indexed edge-property equality first (then
        /// binding the far endpoint), instead of scanning all incident edges.
        indexed_edge_equality: Option<(Str, ScanValue)>,
        /// When set, fixed-label expand filters by the label's inline edge-payload bytes.
        edge_payload_predicate: Option<EdgePayloadPredicate>,
        /// When set, fixed-label expand filters by SIMD vector scoring over inline edge payloads.
        edge_vector_predicate: Option<EdgeVectorPredicate>,
        edge_property_projection: Option<Rc<[Str]>>,
        dst_property_projection: Option<Rc<[Str]>>,
        /// Optional variable bound to a **per-hop auxiliary scalar** after each expansion row
        /// (`Value::Bytes`, `Value::Null`, etc.). Semantics are executor/backend-defined (opaque to the planner).
        /// When [`None`], the executor uses its default auxiliary binding name.
        hop_aux_binding: Option<Str>,
        /// When false, the executor skips binding the traversed `edge` variable.
        emit_edge_binding: bool,
        /// When set with [`var_len`](Self::var_len), bind this node variable to the per-hop
        /// **near** endpoint list (GQL group variable from a quantified subpath).
        near_group_var: Option<Str>,
        /// When set with [`var_len`](Self::var_len), bind this node variable to the per-hop
        /// **far** endpoint list (GQL group variable from a quantified subpath).
        far_group_var: Option<Str>,
        /// When set with [`var_len`](Self::var_len), bind this path variable to the traversed path.
        path_var: Option<Str>,
        /// When false, the executor skips materializing [`path_var`](Self::path_var) (if present).
        emit_path_binding: bool,
    },

    /// Fused Expand + property filter on the destination node (EVFusion).
    /// Avoids materializing intermediate rows that would be discarded.
    ExpandFilter {
        src: Str,
        edge: Str,
        dst: Str,
        direction: EdgeDirection,
        label: Option<EdgeLabelRef>,
        label_expr: Option<LabelExpr>,
        var_len: Option<VarLenSpec>,
        /// When set, same indexed edge-property path as [`PlanOp::Expand`].
        indexed_edge_equality: Option<(Str, ScanValue)>,
        /// When set, same inline edge-payload predicate path as [`PlanOp::Expand`].
        edge_payload_predicate: Option<EdgePayloadPredicate>,
        /// When set, same inline edge-vector predicate path as [`PlanOp::Expand`].
        edge_vector_predicate: Option<EdgeVectorPredicate>,
        /// Predicates evaluated on the destination node during expansion.
        dst_filter: Vec<Expr>,
        edge_property_projection: Option<Rc<[Str]>>,
        dst_property_projection: Option<Rc<[Str]>>,
        /// Same semantics as [`PlanOp::Expand::hop_aux_binding`].
        hop_aux_binding: Option<Str>,
        /// When false, the executor skips binding the traversed `edge` variable.
        emit_edge_binding: bool,
        /// Same semantics as [`PlanOp::Expand::near_group_var`].
        near_group_var: Option<Str>,
        /// Same semantics as [`PlanOp::Expand::far_group_var`].
        far_group_var: Option<Str>,
        /// Same semantics as [`PlanOp::Expand::path_var`].
        path_var: Option<Str>,
        /// Same semantics as [`PlanOp::Expand::emit_path_binding`].
        emit_path_binding: bool,
    },
    ///
    /// `edge` is set to the **last** hop’s edge along each emitted shortest path (or
    /// [`Value::Null`] scalar binding when the path has length zero and `min_hops == 0`).
    ShortestPath {
        src: Str,
        dst: Str,
        edge: Str,
        path_var: Option<Str>,
        /// When false, the executor skips binding the final-hop `edge` variable.
        emit_edge_binding: bool,
        /// When false, the executor skips materializing `path_var` (if present).
        emit_path_binding: bool,
        mode: ShortestMode,
        direction: EdgeDirection,
        label: Option<EdgeLabelRef>,
        /// General edge label predicate (same convention as [`PlanOp::Expand`]). When set, `label` is `None`.
        label_expr: Option<LabelExpr>,
        var_len: Option<VarLenSpec>,
        cost: ShortestPathCost,
    },

    // ──── GQL-specific (not in gleaph-old) ────
    /// LET bindings: compute and bind intermediate values.
    Let { bindings: Vec<LetBinding> },

    /// FOR loop: unnest a list into individual rows.
    For {
        variable: Str,
        list: Expr,
        ordinality: Option<Str>,
        /// When true, `ordinality` is 0-based (`WITH OFFSET`); otherwise 1-based (`WITH ORDINALITY`).
        offset_keyword: bool,
    },

    /// Standalone FILTER statement (GQL §14.2).
    Filter { condition: Expr },

    /// Search a bound graph variable against a provider. Provider-neutral at this layer;
    /// Router/vector-index lowering is a later slice.
    Search {
        binding: Str,
        provider: SearchProviderPlan,
        output: SearchOutputPlan,
    },

    // ──── Procedure calls ────
    /// External procedure call: CALL proc_name(args) [YIELD columns].
    CallProcedure {
        name: Vec<Str>,
        args: Vec<Expr>,
        yield_columns: Option<Vec<YieldColumn>>,
        optional: bool,
    },

    /// Inline procedure call: CALL { <sub-query> }.
    InlineProcedureCall {
        sub_plan: Box<PhysicalPlan>,
        scope: InlineProcedureScope,
        optional: bool,
    },

    /// USE GRAPH: switch graph context for a sub-plan or scope the result.
    UseGraph {
        graph_name: Vec<Str>,
        sub_plan: Option<Vec<PlanOp>>,
    },

    // ──── Join operators ────
    /// Hash join between two independently computed sub-plans.
    HashJoin {
        left: Vec<PlanOp>,
        right: Vec<PlanOp>,
        join_keys: Vec<Str>,
    },

    /// Cartesian product of two independent sub-plans (no shared variables).
    CartesianProduct {
        left: Vec<PlanOp>,
        right: Vec<PlanOp>,
    },

    // ──── Aggregation / Output ────
    /// GROUP BY + aggregate functions.
    Aggregate {
        group_by: Vec<Expr>,
        /// Aggregate expressions evaluated per group.
        aggregates: Vec<AggregateSpec>,
    },

    /// RETURN / SELECT projection.
    Project {
        columns: Vec<ProjectColumn>,
        distinct: bool,
    },

    /// ORDER BY sorting.
    Sort { order_by: OrderByClause },

    /// LIMIT / OFFSET row truncation.
    Limit {
        count: Option<Expr>,
        offset: Option<Expr>,
    },

    /// Set operation combining two sub-plans (UNION, EXCEPT, INTERSECT, OTHERWISE).
    SetOperation {
        op: gleaph_gql::ast::SetOp,
        right: Box<PhysicalPlan>,
    },

    /// Optional match: left-outer-join semantics. If the sub-plan produces no
    /// rows for a given input row, emit the input row with NULLs for the
    /// optional bindings.
    OptionalMatch {
        /// The operators that form the optional sub-plan.
        sub_plan: Vec<PlanOp>,
    },

    /// Intersect multiple index scans on the same variable.
    IndexIntersection {
        variable: Str,
        scans: Vec<IndexScanSpec>,
        property_projection: Option<Rc<[Str]>>,
    },

    /// Worst-Case Optimal Join for cyclic patterns (e.g., triangles).
    /// Replaces multiple Expand ops when a cycle is detected.
    WorstCaseOptimalJoin {
        /// Variables forming the cycle (in traversal order).
        variables: Vec<Str>,
        /// Edge specs for each hop in the cycle.
        edges: Vec<WcojEdge>,
    },

    /// TopK: fused Sort + Limit. Uses a heap instead of full sort
    /// when ORDER BY ... LIMIT N and k << input_rows.
    TopK {
        order_by: OrderByClause,
        k: Expr,
        offset: Option<Expr>,
    },

    // ──── Pipeline boundaries ────
    /// Materialize: pipeline boundary between NEXT-chained statement blocks.
    /// Collects the current result set and re-exposes specified columns.
    /// Analogous to Cypher's WITH or GQL's YIELD between NEXT statements.
    Materialize {
        /// Columns to project/rename at the boundary. Empty = pass all.
        columns: Vec<ProjectColumn>,
        distinct: bool,
    },

    // ──── DML (Data Modification) ────
    /// Insert a new vertex with labels and properties.
    InsertVertex {
        variable: Option<Str>,
        labels: Vec<NodeLabelRef>,
        properties: Vec<PropertyAssignment>,
    },

    /// Insert a new edge between two vertices.
    InsertEdge {
        variable: Option<Str>,
        src: Str,
        dst: Str,
        direction: EdgeDirection,
        labels: Vec<EdgeLabelRef>,
        properties: Vec<PropertyAssignment>,
    },

    /// Set properties or labels on bound variables.
    SetProperties { items: Vec<SetPlanItem> },

    /// Remove properties or labels from bound variables.
    RemoveProperties { items: Vec<RemovePlanItem> },

    /// Delete a vertex (NODETACH — must have no edges).
    DeleteVertex { variable: Str },

    /// Delete a vertex and all its connected edges (DETACH DELETE).
    DetachDeleteVertex { variable: Str },

    /// Delete an edge.
    DeleteEdge { variable: Str },
}

impl PlanOp {
    pub fn is_dml(&self) -> bool {
        matches!(
            self,
            Self::InsertVertex { .. }
                | Self::InsertEdge { .. }
                | Self::SetProperties { .. }
                | Self::RemoveProperties { .. }
                | Self::DeleteVertex { .. }
                | Self::DetachDeleteVertex { .. }
                | Self::DeleteEdge { .. }
        )
    }
}

fn ops_are_pure_insert(ops: &[PlanOp]) -> bool {
    let mut has_insert = false;
    for op in ops {
        match op {
            PlanOp::InsertVertex { .. } | PlanOp::InsertEdge { .. } => has_insert = true,
            // Pure-output boundaries are safe: they neither read graph state nor mutate it.
            PlanOp::Project { .. } | PlanOp::Materialize { .. } => {}
            // Any scan/index/expand/match, non-insert DML, or other operator reads or binds
            // existing state (or is not classifiable as new-only), so the plan is not pure-insert.
            _ => return false,
        }
    }
    has_insert
}

/// True for the leading-anchor operators a router can resolve to a shard set by index lookup: a
/// labeled vertex scan or an indexed (vertex/edge) scan / index intersection. An unlabeled full
/// `NodeScan` and a `ConditionalIndexScan` (which may fall back to a full scan) are excluded —
/// neither pins the read to a bounded, index-resolvable anchor.
fn op_is_seedable_anchor(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::NodeScan { label: Some(_), .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::EdgeIndexScan { .. }
            | PlanOp::IndexIntersection { .. }
    )
}

/// True for any operator that reads or binds existing graph state. The complement (returned
/// false) is the set of *row-local* operators that consume rows produced upstream — pure
/// row-shaping (filter/project/sort/limit/aggregate/let/for/materialize/topk) and DML mutations on
/// already-bound rows or brand-new elements. Any operator not on the row-local allowlist is
/// treated as an existing-state read, so a newly added [`PlanOp`] variant is conservatively
/// excluded from a single-anchor bundle until it is classified.
fn op_reads_existing_graph(op: &PlanOp) -> bool {
    !matches!(
        op,
        PlanOp::PropertyFilter { .. }
            | PlanOp::Filter { .. }
            | PlanOp::Let { .. }
            | PlanOp::For { .. }
            | PlanOp::Aggregate { .. }
            | PlanOp::Project { .. }
            | PlanOp::Sort { .. }
            | PlanOp::Limit { .. }
            | PlanOp::TopK { .. }
            | PlanOp::Materialize { .. }
            | PlanOp::InsertVertex { .. }
            | PlanOp::InsertEdge { .. }
            | PlanOp::SetProperties { .. }
            | PlanOp::RemoveProperties { .. }
            | PlanOp::DeleteVertex { .. }
            | PlanOp::DetachDeleteVertex { .. }
            | PlanOp::DeleteEdge { .. }
    )
}

fn ops_contain_dml(ops: &[PlanOp]) -> bool {
    ops.iter().any(|op| {
        if op.is_dml() {
            return true;
        }
        match op {
            PlanOp::HashJoin { left, right, .. } => ops_contain_dml(left) || ops_contain_dml(right),
            PlanOp::CartesianProduct { left, right } => {
                ops_contain_dml(left) || ops_contain_dml(right)
            }
            PlanOp::SetOperation { right, .. } => ops_contain_dml(&right.ops),
            PlanOp::OptionalMatch { sub_plan } => ops_contain_dml(sub_plan),
            PlanOp::InlineProcedureCall { sub_plan, .. } => ops_contain_dml(&sub_plan.ops),
            PlanOp::UseGraph {
                sub_plan: Some(sp), ..
            } => ops_contain_dml(sp),
            _ => false,
        }
    })
}

fn collect_label_uses_in_ops(ops: &[PlanOp], uses: &mut PlanLabelUses) {
    for op in ops {
        match op {
            PlanOp::NodeScan { label, .. } => {
                if let Some(label) = label {
                    uses.add_node(label, LabelUseIntent::ReadExisting);
                }
            }
            PlanOp::EdgeBindEndpoints { label, .. }
            | PlanOp::Expand { label, .. }
            | PlanOp::ExpandFilter { label, .. }
            | PlanOp::ShortestPath { label, .. } => {
                if let Some(label) = label {
                    uses.add_edge(label, LabelUseIntent::ReadExisting);
                }
            }
            PlanOp::ConditionalIndexScan { fallback_label, .. } => {
                if let Some(label) = fallback_label {
                    uses.add_node(label, LabelUseIntent::ReadExisting);
                }
            }
            PlanOp::InsertVertex { labels, .. } => {
                for label in labels {
                    uses.add_node(label, LabelUseIntent::CreateIfMissing);
                }
            }
            PlanOp::InsertEdge { labels, .. } => {
                for label in labels {
                    uses.add_edge(label, LabelUseIntent::CreateIfMissing);
                }
            }
            PlanOp::SetProperties { items } => {
                for item in items {
                    if let SetPlanItem::Label { label, .. } = item {
                        uses.add_node(label, LabelUseIntent::CreateIfMissing);
                    }
                }
            }
            PlanOp::RemoveProperties { items } => {
                for item in items {
                    if let RemovePlanItem::Label { label, .. } = item {
                        uses.add_node(label, LabelUseIntent::ReadExisting);
                    }
                }
            }
            PlanOp::WorstCaseOptimalJoin { edges, .. } => {
                for edge in edges {
                    if let Some(label) = &edge.label {
                        uses.add_edge(label, LabelUseIntent::ReadExisting);
                    }
                }
            }
            PlanOp::HashJoin { left, right, .. } => {
                collect_label_uses_in_ops(left, uses);
                collect_label_uses_in_ops(right, uses);
            }
            PlanOp::CartesianProduct { left, right } => {
                collect_label_uses_in_ops(left, uses);
                collect_label_uses_in_ops(right, uses);
            }
            PlanOp::SetOperation { right, .. } => {
                collect_label_uses_in_ops(&right.ops, uses);
            }
            PlanOp::OptionalMatch { sub_plan } => collect_label_uses_in_ops(sub_plan, uses),
            PlanOp::InlineProcedureCall { sub_plan, .. } => {
                collect_label_uses_in_ops(&sub_plan.ops, uses);
            }
            PlanOp::UseGraph {
                sub_plan: Some(sub_plan),
                ..
            } => collect_label_uses_in_ops(sub_plan, uses),
            PlanOp::IndexScan { .. }
            | PlanOp::EdgeIndexScan { .. }
            | PlanOp::PropertyFilter { .. }
            | PlanOp::Let { .. }
            | PlanOp::For { .. }
            | PlanOp::Filter { .. }
            | PlanOp::Search { .. }
            | PlanOp::CallProcedure { .. }
            | PlanOp::UseGraph { sub_plan: None, .. }
            | PlanOp::Aggregate { .. }
            | PlanOp::Project { .. }
            | PlanOp::Sort { .. }
            | PlanOp::Limit { .. }
            | PlanOp::IndexIntersection { .. }
            | PlanOp::TopK { .. }
            | PlanOp::Materialize { .. }
            | PlanOp::DeleteVertex { .. }
            | PlanOp::DetachDeleteVertex { .. }
            | PlanOp::DeleteEdge { .. } => {}
        }
    }
}

fn collect_property_names_from_expr(expr: &Expr, uses: &mut PlanPropertyUses) {
    match &expr.kind {
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            for (name, _) in fields {
                let name: Str = name.as_str().into();
                uses.add_property(&name, PropertyUseIntent::CreateIfMissing);
            }
        }
        _ => {}
    }
}

fn collect_read_properties_from_expr(expr: &Expr, uses: &mut PlanPropertyUses) {
    if let ExprKind::PropertyAccess { property, .. } = &expr.kind {
        uses.add_property(
            &Str::from(property.as_str()),
            PropertyUseIntent::ReadExisting,
        );
    }
    crate::expr_children::for_each_immediate_child_expr(expr, |child| {
        collect_read_properties_from_expr(child, uses);
    });
}

fn add_property_projection(uses: &mut PlanPropertyUses, projection: Option<&[Str]>) {
    if let Some(properties) = projection {
        for name in properties {
            uses.add_property(name, PropertyUseIntent::ReadExisting);
        }
    }
}

fn collect_property_uses_in_ops(ops: &[PlanOp], uses: &mut PlanPropertyUses) {
    for op in ops {
        match op {
            PlanOp::NodeScan {
                property_projection,
                ..
            } => add_property_projection(uses, property_projection.as_deref()),
            PlanOp::IndexScan {
                property,
                property_projection,
                ..
            } => {
                uses.add_property(property, PropertyUseIntent::ReadExisting);
                add_property_projection(uses, property_projection.as_deref());
            }
            PlanOp::EdgeIndexScan {
                property,
                property_projection,
                ..
            } => {
                uses.add_property(property, PropertyUseIntent::ReadExisting);
                add_property_projection(uses, property_projection.as_deref());
            }
            PlanOp::ConditionalIndexScan {
                candidates,
                property_projection,
                ..
            } => {
                for candidate in candidates {
                    uses.add_property(&candidate.property, PropertyUseIntent::ReadExisting);
                }
                add_property_projection(uses, property_projection.as_deref());
            }
            PlanOp::IndexIntersection {
                scans,
                property_projection,
                ..
            } => {
                for spec in scans {
                    uses.add_property(&spec.property, PropertyUseIntent::ReadExisting);
                }
                add_property_projection(uses, property_projection.as_deref());
            }
            PlanOp::Expand {
                indexed_edge_equality,
                edge_property_projection,
                dst_property_projection,
                ..
            }
            | PlanOp::ExpandFilter {
                indexed_edge_equality,
                edge_property_projection,
                dst_property_projection,
                ..
            } => {
                if let Some((property, _)) = indexed_edge_equality {
                    uses.add_property(property, PropertyUseIntent::ReadExisting);
                }
                add_property_projection(uses, edge_property_projection.as_deref());
                add_property_projection(uses, dst_property_projection.as_deref());
            }
            PlanOp::EdgeBindEndpoints {
                near_property_projection,
                far_property_projection,
                ..
            } => {
                add_property_projection(uses, near_property_projection.as_deref());
                add_property_projection(uses, far_property_projection.as_deref());
            }
            PlanOp::InsertVertex { properties, .. } | PlanOp::InsertEdge { properties, .. } => {
                for assignment in properties {
                    uses.add_property(&assignment.name, PropertyUseIntent::CreateIfMissing);
                }
            }
            PlanOp::SetProperties { items } => {
                for item in items {
                    match item {
                        SetPlanItem::Property { property, .. } => {
                            uses.add_property(property, PropertyUseIntent::CreateIfMissing);
                        }
                        SetPlanItem::AllProperties { value, .. } => {
                            collect_property_names_from_expr(value, uses);
                        }
                        SetPlanItem::Label { .. } => {}
                    }
                }
            }
            PlanOp::RemoveProperties { items } => {
                for item in items {
                    if let RemovePlanItem::Property { property, .. } = item {
                        uses.add_property(property, PropertyUseIntent::ReadExisting);
                    }
                }
            }
            PlanOp::HashJoin { left, right, .. } => {
                collect_property_uses_in_ops(left, uses);
                collect_property_uses_in_ops(right, uses);
            }
            PlanOp::CartesianProduct { left, right } => {
                collect_property_uses_in_ops(left, uses);
                collect_property_uses_in_ops(right, uses);
            }
            PlanOp::SetOperation { right, .. } => {
                collect_property_uses_in_ops(&right.ops, uses);
            }
            PlanOp::OptionalMatch { sub_plan } => collect_property_uses_in_ops(sub_plan, uses),
            PlanOp::InlineProcedureCall { sub_plan, .. } => {
                collect_property_uses_in_ops(&sub_plan.ops, uses);
            }
            PlanOp::UseGraph {
                sub_plan: Some(sub_plan),
                ..
            } => collect_property_uses_in_ops(sub_plan, uses),
            PlanOp::WorstCaseOptimalJoin { edges, .. } => {
                for edge in edges {
                    if let Some((property, _)) = &edge.indexed_edge_equality {
                        uses.add_property(property, PropertyUseIntent::ReadExisting);
                    }
                }
            }
            PlanOp::PropertyFilter { predicates, .. } => {
                for predicate in predicates {
                    collect_read_properties_from_expr(predicate, uses);
                }
            }
            PlanOp::Filter { condition, .. } => {
                collect_read_properties_from_expr(condition, uses);
            }
            PlanOp::Search { provider, .. } => {
                collect_read_properties_from_expr(provider.query(), uses);
                collect_read_properties_from_expr(provider.limit(), uses);
                if let Some(filter) = provider.filter() {
                    collect_read_properties_from_expr(filter, uses);
                }
            }
            _ => {}
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Supporting types
// ════════════════════════════════════════════════════════════════════════════════

/// A property assignment for INSERT/SET operations.
#[derive(Clone, Debug)]
pub struct PropertyAssignment {
    pub name: Str,
    pub value: Expr,
}

/// A single SET plan item.
#[derive(Clone, Debug)]
pub enum SetPlanItem {
    /// SET v.property = value
    Property {
        variable: Str,
        property: Str,
        value: Expr,
    },
    /// SET v = expr (replace all properties)
    AllProperties { variable: Str, value: Expr },
    /// SET v IS Label
    Label { variable: Str, label: NodeLabelRef },
}

/// A single REMOVE plan item.
#[derive(Clone, Debug)]
pub enum RemovePlanItem {
    /// REMOVE v.property
    Property { variable: Str, property: Str },
    /// REMOVE v IS Label
    Label { variable: Str, label: NodeLabelRef },
}

/// A value used in index scan predicates.
#[derive(Clone, Debug, PartialEq)]
pub enum ScanValue {
    /// A literal constant.
    Literal(gleaph_gql::Value),
    /// A query parameter reference.
    Parameter(Str),
}

// CmpOp is re-exported from gleaph_gql::ast::CmpOp.

/// A comparison that can be evaluated directly against fixed-width edge-payload bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgePayloadPredicate {
    pub op: CmpOp,
    pub value: ScanValue,
}

/// Vector metric evaluated directly against fixed-width `VectorF32` edge-payload bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeVectorMetric {
    Dot,
    L2Squared,
    CosineDistance,
}

/// A vector score comparison that can be evaluated directly against edge-payload bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeVectorPredicate {
    pub metric: EdgeVectorMetric,
    pub query: ScanValue,
    pub op: CmpOp,
    pub threshold: ScanValue,
}

/// Search provider in a physical plan. Provider-neutral: no Router / vector-canister
/// semantics are resolved here.
#[derive(Clone, Debug, PartialEq)]
pub enum SearchProviderPlan {
    VectorIndex {
        index_name: Vec<Str>,
        query: Expr,
        limit: Expr,
        filter: Option<Expr>,
    },
}

impl SearchProviderPlan {
    pub fn query(&self) -> &Expr {
        match self {
            Self::VectorIndex { query, .. } => query,
        }
    }

    pub fn limit(&self) -> &Expr {
        match self {
            Self::VectorIndex { limit, .. } => limit,
        }
    }

    pub fn filter(&self) -> Option<&Expr> {
        match self {
            Self::VectorIndex { filter, .. } => filter.as_ref(),
        }
    }
}

/// Output alias for a `PlanOp::Search`.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchOutputPlan {
    pub kind: SearchOutputKind,
    pub alias: Str,
}

/// Whether a search output alias represents a score or a distance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchOutputKind {
    Score,
    Distance,
}

/// Variable-length expansion bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarLenSpec {
    pub min: u64,
    pub max: Option<u64>,
}

/// Cost model for shortest-path search.
#[derive(Clone, Debug, PartialEq)]
pub enum ShortestPathCost {
    /// Unweighted hop count (breadth-first).
    HopCount,
    /// Per-hop edge cost from an extension expression; total path cost is the sum.
    EdgeCostExpr { edge_var: Str, expr: Expr },
}

/// Shortest-path mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortestMode {
    /// Any single shortest path.
    AnyShortest,
    /// All shortest paths.
    AllShortest,
    /// Up to k shortest paths (one output row per path).
    ShortestK(u64),
    /// Up to k shortest paths grouped into one row (`SHORTEST k GROUP`).
    ShortestKGroup(u64),
}

impl ShortestMode {
    /// Hop-count / weighted k-shortest limit shared by [`ShortestK`] and [`ShortestKGroup`].
    pub fn shortest_k_limit(self) -> Option<u64> {
        match self {
            Self::ShortestK(k) | Self::ShortestKGroup(k) => Some(k),
            _ => None,
        }
    }

    /// When true, path and edge bindings are emitted as groups on a single row.
    pub fn emits_path_group(self) -> bool {
        matches!(self, Self::ShortestKGroup(_))
    }
}

/// A candidate for conditional index scan.
#[derive(Clone, Debug)]
pub struct ConditionalScanCandidate {
    /// Parameter name to check for null.
    pub param_name: Str,
    /// Property to scan on.
    pub property: Str,
    /// Variable to bind.
    pub variable: Str,
    /// Comparison operator.
    pub cmp: CmpOp,
}

/// An aggregate specification within an Aggregate op.
#[derive(Clone, Debug, PartialEq)]
pub struct AggregateSpec {
    /// Aggregate function (typed; mirrors [`gleaph_gql::ast::ExprKind::Aggregate`]).
    pub func: AggregateFunc,
    /// Primary aggregated expression (`None` for `COUNT(*)` / [`AggregateFunc::CountStar`]).
    pub expr: Option<Expr>,
    /// Second argument (e.g. percentile fraction for `PERCENTILE_CONT` / `PERCENTILE_DISC`).
    pub expr2: Option<Expr>,
    /// Whether `DISTINCT` is applied.
    pub distinct: bool,
    /// Optional `FILTER (WHERE ...)` predicate.
    pub filter: Option<Expr>,
    /// Optional aggregate-local `ORDER BY` (e.g. `COLLECT_LIST` ordering).
    pub order_by: Option<OrderByClause>,
    /// Output alias.
    pub alias: Option<Str>,
}

/// A column in a Project op.
#[derive(Clone, Debug)]
pub struct ProjectColumn {
    pub expr: Expr,
    pub alias: Option<Str>,
}

// ════════════════════════════════════════════════════════════════════════════════
// PlanAnnotations
// ════════════════════════════════════════════════════════════════════════════════

/// Metadata and annotations produced during planning.
#[derive(Clone, Debug, Default)]
pub struct PlanAnnotations {
    /// Semantic analysis metadata surfaced for explain/debugging.
    pub semantic: SemanticPlanAnnotations,
    /// Optimizer and planning metadata used for costing and execution hints.
    pub optimizer: OptimizerPlanAnnotations,
}

/// Semantic analysis metadata produced during planning.
#[derive(Clone, Debug, Default)]
pub struct SemanticPlanAnnotations {
    /// All property accesses detected in the query (e.g. "n.name", "e.weight").
    pub property_accesses: Option<Vec<Str>>,
    /// Property accesses in WHERE clauses only.
    pub where_property_accesses: Option<Vec<Str>>,
    /// WHERE properties that have indexes available.
    pub indexable_properties: Option<Vec<Str>>,
    /// Whether the query uses aggregate functions.
    pub has_aggregate: bool,
    /// Flow-sensitive narrowing facts.
    pub narrowing_facts: Option<Vec<crate::semantic::NarrowingFact>>,
}

/// Optimizer and planning metadata produced during planning.
#[derive(Clone, Debug, Default)]
pub struct OptimizerPlanAnnotations {
    /// The variable chosen as the scan anchor (starting point).
    pub anchor: Option<AnchorInfo>,
    /// Estimated result row count.
    pub estimated_rows: Option<f64>,
    /// Estimated total cost (arbitrary unit).
    pub estimated_cost: Option<f64>,
    /// Whether limit pushdown was applied.
    pub limit_pushdown_applied: bool,
    /// Filter pushdown stages (which predicates were pushed to which stage).
    pub filter_pushdown_stages: Vec<usize>,
    /// Whether the plan is statically contradictory (unsatisfiable pattern).
    pub statically_contradictory: bool,
    /// Recommended hop execution order (indices into the hops array).
    pub join_order: Option<Vec<usize>>,
    /// Whether EVFusion (Expand-Vertex filter fusion) was applied.
    pub ev_fusion_applied: bool,
    /// Whether TopK fusion (Sort+Limit → TopK) was applied.
    pub topk_applied: bool,
    /// Whether predicate reordering by selectivity was applied.
    pub predicate_reordering_applied: bool,
    /// Common subexpressions detected across the plan (annotation-only).
    pub common_subexpressions: Option<Vec<Str>>,
    /// Suggested row count after which executor should re-evaluate the plan.
    pub reoptimize_after_rows: Option<u64>,
    /// Op indices where executor should verify cardinality matches estimates.
    pub cardinality_check_points: Vec<usize>,
    /// Whether late projection optimization was applied.
    pub late_project_applied: bool,
    /// Detected cyclic patterns (e.g., triangles) for potential WCOJ.
    pub cyclic_patterns: Option<Vec<CyclicPattern>>,
    /// Remote `USE GRAPH` pushdown capability analysis per encountered focused graph.
    pub use_graph_pushdown: Vec<UseGraphPushdownInfo>,
}

/// A detected cyclic pattern in a graph query (e.g., triangle).
#[derive(Clone, Debug)]
pub struct CyclicPattern {
    /// Variables forming the cycle.
    pub variables: Vec<Str>,
}

/// Planner-side summary of whether one `USE GRAPH` sub-plan can be translated for remote pushdown.
#[derive(Clone, Debug)]
pub struct UseGraphPushdownInfo {
    pub graph_name: String,
    pub supported: bool,
    pub reason: Option<String>,
}

/// A column yielded from a procedure call.
#[derive(Clone, Debug)]
pub struct YieldColumn {
    pub name: Str,
    pub alias: Option<Str>,
}

/// A single index scan specification for index intersection.
#[derive(Clone, Debug)]
pub struct IndexScanSpec {
    pub property: Str,
    pub value: ScanValue,
    pub cmp: CmpOp,
}

/// An edge in a WCOJ plan: directed hop from pattern `src` to `dst` (cycle closes on last→first).
#[derive(Clone, Debug)]
pub struct WcojEdge {
    pub src: Str,
    pub dst: Str,
    pub variable: Str,
    pub label: Option<EdgeLabelRef>,
    pub label_expr: Option<LabelExpr>,
    pub direction: EdgeDirection,
    /// Variable-length segment (`None` = exactly one edge).
    pub var_len: Option<VarLenSpec>,
    /// Indexed edge equality (mutually exclusive with `var_len` in the planner).
    pub indexed_edge_equality: Option<(Str, ScanValue)>,
    /// Predicates on destination `dst` (from `ExpandFilter` / pattern fusion).
    pub dst_filter: Vec<Expr>,
    /// When set, executor binds hop auxiliary bytes under this name (same as [`PlanOp::Expand::hop_aux_binding`]).
    pub hop_aux_binding: Option<Str>,
}

/// Information about the chosen scan anchor.
#[derive(Clone, Debug)]
pub struct AnchorInfo {
    /// The variable name chosen as anchor.
    pub variable: Str,
    /// Why this anchor was chosen.
    pub source: AnchorSource,
}

/// The reason an anchor was chosen.
#[derive(Clone, Debug)]
pub enum AnchorSource {
    /// Equality predicate on an indexed property (best selectivity).
    PropertyEquality { property: Str },
    /// Inline property from pattern: `(n:Label {prop: value})` or `(n WHERE n.prop = value)`.
    InlinePropertyEquality { property: Str },
    /// Range predicate on an indexed property.
    PropertyRange {
        property: Str,
        value: ScanValue,
        cmp: CmpOp,
    },
    /// Lowest-cardinality label.
    LabelCardinality { label: NodeLabelRef },
    /// Schema-inferred endpoint.
    SchemaEndpoint,
    /// Full scan fallback.
    FullScan,
}

#[cfg(test)]
mod property_uses_tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{CmpOp, Expr, ExprKind};

    #[test]
    fn property_filter_contributes_read_property_uses() {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("n".into()))),
                        property: "age".into(),
                    })),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(5)))),
                })],
                stage: 0,
            },
        ]);
        let uses = plan.property_uses();
        assert_eq!(
            uses.properties.get("age" as &str),
            Some(&PropertyUseIntent::ReadExisting)
        );
    }
}
