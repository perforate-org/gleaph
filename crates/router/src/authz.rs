//! Plan-time data-plane enforcement (ADR 0074 slice 2b).
//!
//! Walks the built [`PhysicalPlan`] of one resolved graph on the Router and extracts the
//! exact data-plane privilege rows the plan demands ([`RequirementSet`]), then evaluates
//! those rows against the caller's effective privileges (`caller ∪ PUBLIC`, expiry-aware;
//! ownership through the implicit-root coverage arm, ADR 0074 §3 invariant 3). Any
//! uncovered demand fails the whole query with the uniform non-disclosing
//! [`RouterError::Forbidden`] that never names the missing privilege or resource.
//!
//! Boundary rules:
//! - `gleaph-gql` / `gleaph-gql-planner` stay generic; this module lives in the Router and
//!   consumes only their public types. Catalog ids are resolved through the graph-scoped
//!   catalogs ([`RouterStore`]) at extraction time.
//! - Every [`PlanOp`] variant is matched exhaustively with no wildcard op arm, so a future
//!   planner variant fails compilation here instead of silently bypassing enforcement.
//! - Administrative capabilities are not an input to evaluation anywhere in this module
//!   (ADR 0074 invariant 1: authority never implies data access).
//! - The extracted [`RequirementSet`] is also the durable artifact of ADR 0074 slice 3: it
//!   is embedded in the prepared-query record at registration, gates invariant-7-bounded
//!   publication, and is the primary checked set at prepared execution (the live walk
//!   remains the fallback). It is therefore `pub` and serializable while this module stays
//!   its single owner.
//!
//! Phase-1 attribution contract (fail closed where the plan cannot name a resource):
//! - Labeled scans demand `MATCH` on the vertex label; a retained full property map
//!   (`property_projection: None`) additionally demands `READ` on that label; an explicit
//!   non-empty projection demands `READ_PROPERTY` per listed key; an empty projection
//!   demands nothing beyond `MATCH`.
//! - An index-anchored vertex scan binds only vertices of the probe property's bound
//!   label: when the catalog resolves a unique Active vertex index for the property
//!   (same uniqueness the executor applies to its posting namespace), the anchor notes
//!   that label fact and demands the same rows as a labeled scan. Zero or multiple
//!   active namespaces — or resolutions that disagree with a conditional scan's fallback
//!   label — leave the anchor unattributed (fail closed).
//! - Traversal demands derive from pattern direction × schema directedness: declared
//!   directed labels probe one directional row per orientation (undirected patterns over a
//!   directed label need both), while undirected or undeclared labels take the unoriented
//!   row — mirroring how GRANT lowers rows (`gql_grants::lower_privileges`). Edge-inline
//!   property bytes ride the edge-label traversal row; dedicated edge-property resources
//!   do not exist in Phase 1.
//! - Reads that cannot be attributed to exactly one label (unlabeled scans, ambiguous
//!   variables, unresolvable open-schema names, `NOT`/wildcard label expressions) mark the
//!   demand set tenancy-only: owners and admins proceed, everyone else is denied. Phase 1
//!   grants enumerate labels, so a wildcard read is not expressible as a grant.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use candid::{CandidType, Principal};
use gleaph_auth::{
    Direction, GrantSubject, GraphOperation, GraphPrivilege, GraphResource, MetadataScope,
    Privilege,
};
use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql::type_check::PropertySchema as _;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::expr_children::for_each_immediate_child_expr;
use gleaph_gql_planner::plan::{
    EdgeLabelRef, NodeLabelRef, PhysicalPlan, PlanOp, RemovePlanItem, SearchProviderPlan,
    SetPlanItem, ShortestPathCost,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId, PropertyId, VertexLabelId};

use crate::facade::stable::graph_catalog;
use crate::facade::store::RouterStore;
use crate::state::RouterError;

// ──── Catalog resolution ────

/// Graph-scoped name → catalog-id resolution used while extracting requirements.
///
/// Abstracted so host unit tests can drive the walker without Router stable state.
pub(crate) trait PlanCatalogView {
    fn graph_by_name(&self, name: &str) -> Option<GraphId>;
    fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32>;
    /// `(label id, declared UNDIRECTED)`; `None` for the boolean when the graph type
    /// schema does not declare the label's directedness (open-schema graphs).
    fn edge_label(&self, graph: GraphId, name: &str) -> Option<(u32, Option<bool>)>;
    fn property(&self, graph: GraphId, name: &str) -> Option<u32>;
    /// Source facts of a registered vector index (ADR 0078 §1): the creation-fixed label
    /// set it spans and the property id of its embedding source when that embedding name is
    /// backed by a projected graph property. `None` for an unregistered index name.
    fn vector_index_source(&self, graph: GraphId, index_name: &str) -> Option<VectorIndexSource>;
    /// Label name bound by the unique Active vertex property index on `property_id`
    /// (ADR 0009): the same uniqueness resolution the executor applies when it lowers a
    /// property-anchored probe to a posting namespace. Zero or multiple active namespaces
    /// resolve to `None` so callers keep the read unattributed (fail closed).
    fn vertex_property_index_label(&self, _graph: GraphId, _property_id: u32) -> Option<String> {
        // Default keeps ad-hoc test catalogs fail-closed: no index-derived label fact.
        None
    }
}

/// Vector-index search source facts for layer-1 requirement extraction (ADR 0078 §1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VectorIndexSource {
    /// Creation-fixed label set the index spans, as `(label id, label name)` pairs so the
    /// walker can both demand rows on the ids and record the names as scope label facts.
    pub(crate) labels: Vec<(u32, String)>,
    /// Property id of the projected embedding source property (`CREATE VECTOR INDEX … ON
    /// d.embedding` interns the embedding under the property identifier). `None` for an
    /// ingestion-fed embedding field that is not a graph property — no value read exists
    /// to demand in that case.
    pub(crate) embedding_property_id: Option<u32>,
}

/// Production view over the Router graph-scoped catalogs.
struct RouterCatalogView<'a> {
    store: &'a RouterStore,
}

impl PlanCatalogView for RouterCatalogView<'_> {
    fn graph_by_name(&self, name: &str) -> Option<GraphId> {
        graph_catalog::lookup_graph_id(name)
    }

    fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32> {
        self.store
            .lookup_vertex_label_id(graph, name)
            .ok()
            .map(|id| u32::from(id.raw()))
    }

    fn edge_label(&self, graph: GraphId, name: &str) -> Option<(u32, Option<bool>)> {
        let id = self.store.lookup_edge_label_id(graph, name).ok()?;
        let undirected =
            crate::facade::stable::graph_type_catalog::try_property_schema_for_graph_id(graph)
                .ok()
                .flatten()
                .and_then(|schema| schema.edge_is_undirected(name));
        Some((u32::from(id.raw()), undirected))
    }

    fn property(&self, graph: GraphId, name: &str) -> Option<u32> {
        self.store
            .lookup_property_id(graph, name)
            .ok()
            .map(|id| id.raw())
    }

    fn vector_index_source(&self, graph: GraphId, index_name: &str) -> Option<VectorIndexSource> {
        let index_name_id =
            crate::facade::stable::index_name_catalog::lookup_index_name_id(graph, index_name)?;
        let def = crate::facade::stable::vector_index_catalog::get_vector_index_by_name_id(
            graph,
            index_name_id,
        )?;
        // The embedding source property exists only when the embedding name is the
        // projected property identifier of a semantic `CREATE VECTOR INDEX`; legacy
        // ingestion-fed field names resolve to no property and demand no value read.
        let embedding_property_id = crate::facade::stable::embedding_name_catalog::embedding_name(
            graph,
            def.embedding_name_id,
        )
        .and_then(|name| self.property(graph, &name));
        let labels = def
            .labels
            .iter()
            .filter_map(|label_id| {
                self.store
                    .reverse_vertex_label_name(graph, *label_id)
                    .ok()
                    .map(|name| (u32::from(label_id.raw()), name))
            })
            .collect();
        Some(VectorIndexSource {
            labels,
            embedding_property_id,
        })
    }

    fn vertex_property_index_label(&self, graph: GraphId, property_id: u32) -> Option<String> {
        let label_id = crate::facade::stable::indexed_catalog::unique_active_vertex_index_label(
            graph,
            PropertyId::from_raw(property_id),
        )?;
        self.store
            .reverse_vertex_label_name(graph, VertexLabelId::from_raw(label_id))
            .ok()
    }
}

// ──── Requirement model ────

/// One conjunctive group of rows. Inside [`GraphDemands::alternatives`] the groups are
/// OR-alternatives (e.g. `-[:A|B]->`): satisfying any one group satisfies the demand.
type RowGroup = Vec<Privilege>;

/// One graph's extracted data-plane demands.
#[derive(
    Default, Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize,
)]
struct GraphDemands {
    /// Rows that must ALL be covered.
    conjunctive: Vec<Privilege>,
    /// Alternation groups: at least one fully covered group admits the demand.
    alternatives: Vec<RowGroup>,
    /// Set when some read could not be attributed to exactly one grantable resource; only
    /// implicit-root (registry) coverage admits such a plan (Phase 1 grants enumerate labels).
    unattributed: bool,
    /// Marker for an unconstrained or positive multi-label vertex scan with an empty
    /// property projection ([ADR 0089] §2). When set, evaluation requires the caller to
    /// hold `MATCH` on at least one vertex label (or the wildcard `NODES *` row); the
    /// per-bucket restriction is then applied at request build. When unset, an
    /// unconstrained scan with property reads stays `unattributed` (tenancy-only, fail
    /// closed) because property values are atomic.
    unconstrained_vertex_match: bool,
}

/// Every data-plane demand of one plan, grouped by target graph.
///
/// Single representation of a plan's static authorization requirements (ADR 0074 §4):
/// produced by [`extract`], embedded in the prepared-query record at registration
/// (slice 3), evaluated by [`authorize_requirements`] at enforcement and publication time.
#[derive(
    Default, Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize,
)]
pub struct RequirementSet {
    graphs: BTreeMap<u32, GraphDemands>,
    /// Set when a demand cannot even be pinned to a graph (unresolvable `USE GRAPH`
    /// target); admitted only through ownership on the executing root graph.
    root_unattributed: bool,
}

impl RequirementSet {
    fn demands(&mut self, graph: GraphId) -> &mut GraphDemands {
        self.graphs.entry(graph.raw()).or_default()
    }

    /// Test-only summary for the GAP-008 diagnosis (plan 0303).
    #[cfg(test)]
    pub(crate) fn test_demand_summary(&self, graph_raw: u32) -> Option<(bool, usize, usize)> {
        self.graphs
            .get(&graph_raw)
            .map(|d| (d.unattributed, d.conjunctive.len(), d.alternatives.len()))
    }

    /// Test-only summary of the unconstrained-vertex-match marker for a graph.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_unconstrained_vertex_match(&self, graph_raw: u32) -> bool {
        self.graphs
            .get(&graph_raw)
            .is_some_and(|d| d.unconstrained_vertex_match)
    }

    /// Test-only clones of one graph's conjunctive rows, for exact-multiset contract
    /// locks (plan 0306); [`Privilege`] already implements `PartialEq`, so equality
    /// needs no parallel summary vocabulary.
    #[cfg(test)]
    pub(crate) fn test_graph_rows(&self, graph_raw: u32) -> Option<Vec<Privilege>> {
        self.graphs.get(&graph_raw).map(|d| d.conjunctive.clone())
    }

    fn require(&mut self, graph: GraphId, privilege: Privilege) {
        self.demands(graph).conjunctive.push(privilege);
    }

    fn require_all(&mut self, graph: GraphId, privileges: RowGroup) {
        self.demands(graph).conjunctive.extend(privileges);
    }

    fn require_alternatives(&mut self, graph: GraphId, groups: Vec<RowGroup>) {
        self.demands(graph).alternatives.extend(groups);
    }

    fn require_unattributed(&mut self, graph: GraphId) {
        self.demands(graph).unattributed = true;
    }

    /// Marker demand for an unconstrained or positive multi-label vertex scan with an
    /// empty property projection ([ADR 0089] §2). The caller must hold `MATCH` on at
    /// least one vertex label; the per-bucket restriction is then applied at request
    /// build. Property projections stay `unattributed` (tenancy-only) because property
    /// values are atomic.
    fn require_unconstrained_vertex_match(&mut self, graph: GraphId) {
        self.demands(graph).unconstrained_vertex_match = true;
    }
}

fn vertex_row(graph: u32, operation: GraphOperation, label_id: u32) -> Privilege {
    Privilege::Graph(GraphPrivilege {
        graph,
        operation,
        resource: GraphResource::VertexLabel(label_id),
    })
}

fn vertex_property_row(graph: u32, label_id: u32, property_id: u32) -> Privilege {
    Privilege::Graph(GraphPrivilege {
        graph,
        operation: GraphOperation::ReadProperty,
        resource: GraphResource::VertexProperty {
            label: label_id,
            property: property_id,
        },
    })
}

fn edge_row(graph: u32, operation: GraphOperation, label_id: u32) -> Privilege {
    Privilege::Graph(GraphPrivilege {
        graph,
        operation,
        resource: GraphResource::EdgeLabel(label_id),
    })
}

/// Compute the set of vertex variables whose physical properties are read by some
/// downstream expression in the plan ([ADR 0089] §4, Gap 2 follow-up). A variable is
/// "property-reading" if any expression reachable from the plan output references a
/// `PropertyAccess` rooted at that variable, or hydrates the variable as a vertex record
/// (bare `Variable(v)` reference in the output, or a property-filter that reads a named
/// property off the variable).
///
/// Variables that are only consumed by `ElementId(v)`, `count(v)`, `id(v)`, label
/// predicates, or traversal bindings (e.g., the source of an `Expand`) are not
/// property-reading: the executor needs only the vertex identity, not its property
/// values, to serve them. Such variables are *topology-only* and the unconstrained
/// scan that binds them takes the marker + per-bucket path, not the tenancy-only
/// full-map path.
fn collect_property_reading_vars(plan: &PhysicalPlan) -> std::collections::BTreeSet<Rc<str>> {
    let mut out = std::collections::BTreeSet::new();
    for op in &plan.ops {
        collect_property_reading_vars_in_op(op, &mut out);
    }
    out
}

fn collect_property_reading_vars_in_op(op: &PlanOp, out: &mut std::collections::BTreeSet<Rc<str>>) {
    match op {
        PlanOp::Filter { condition } => {
            if expr_reads_vertex_property(condition) {
                mark_expr_vars_as_reading(condition, out);
            }
        }
        PlanOp::PropertyFilter { predicates, .. } => {
            for pred in predicates {
                mark_expr_vars_as_reading(pred, out);
            }
        }
        PlanOp::ExpandFilter { dst_filter, .. } => {
            for pred in dst_filter {
                mark_expr_vars_as_reading(pred, out);
            }
        }
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
            for col in columns {
                if col_has_vertex_hydration(col)
                    && let Some(var) = project_column_var(col)
                {
                    out.insert(Rc::from(var.as_str()));
                }
                mark_expr_vars_as_reading(&col.expr, out);
            }
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for expr in group_by {
                mark_expr_vars_as_reading(expr, out);
            }
            for agg in aggregates {
                if let Some(expr) = &agg.expr {
                    mark_expr_vars_as_reading(expr, out);
                }
                if let Some(expr) = &agg.filter {
                    mark_expr_vars_as_reading(expr, out);
                }
                if let Some(ob) = &agg.order_by {
                    for item in &ob.items {
                        mark_expr_vars_as_reading(&item.expr, out);
                    }
                }
            }
        }
        PlanOp::Sort { order_by } => {
            for item in &order_by.items {
                mark_expr_vars_as_reading(&item.expr, out);
            }
        }
        PlanOp::TopK { order_by, .. } => {
            for item in &order_by.items {
                mark_expr_vars_as_reading(&item.expr, out);
            }
        }
        PlanOp::Let { bindings } => {
            for b in bindings {
                mark_expr_vars_as_reading(&b.value, out);
            }
        }
        PlanOp::For { list, .. } => {
            mark_expr_vars_as_reading(list, out);
        }
        _ => {}
    }
}

/// Whether the expression reads a physical vertex property. A `PropertyAccess` rooted
/// at any variable, or any expression that recursively contains one, qualifies.
fn expr_reads_vertex_property(expr: &Expr) -> bool {
    expr_contains_property_access(expr)
}

fn expr_contains_property_access(expr: &Expr) -> bool {
    if matches!(expr.kind, ExprKind::PropertyAccess { .. }) {
        return true;
    }
    let mut found = false;
    for_each_immediate_child_expr(expr, |child| {
        if !found && expr_contains_property_access(child) {
            found = true;
        }
    });
    found
}

/// Mark every vertex variable reachable through `expr` that participates in a
/// property-reading sub-expression. A bare `Variable(v)` reference does NOT
/// automatically mark `v` here — the caller decides via [`col_has_vertex_hydration`]
/// for `Project` columns whether the variable is being hydrated as a vertex record.
fn mark_expr_vars_as_reading(expr: &Expr, out: &mut std::collections::BTreeSet<Rc<str>>) {
    if let Some(root) = property_access_root(expr) {
        out.insert(Rc::from(root.as_str()));
    }
}

/// If `expr` is or contains a `PropertyAccess`, return the root variable name of the
/// leftmost access chain. Otherwise `None`. A bare `Variable(v)` reference is NOT a
/// property access — it must be detected separately as a vertex-hydration.
fn property_access_root(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::PropertyAccess { expr: inner, .. } => property_access_root(inner),
        _ => None,
    }
}

/// Whether a `ProjectColumn` hydrates a vertex record (bare `Variable(v)` reference with
/// no alias) — this is the shape `RETURN a` and `RETURN b, c` take, and it forces a full
/// property read.
fn col_has_vertex_hydration(col: &gleaph_gql_planner::plan::ProjectColumn) -> bool {
    matches!(col.expr.kind, ExprKind::Variable(_)) && col.alias.is_none()
}

fn project_column_var(col: &gleaph_gql_planner::plan::ProjectColumn) -> Option<String> {
    match &col.expr.kind {
        ExprKind::Variable(v) => Some(v.to_string()),
        _ => None,
    }
}

// ──── Extraction ────

/// Per-scope variable label facts accumulated while walking one op list.
struct Scope {
    graph: GraphId,
    /// Vertex variable → label-name set seen at binding or `IS LABELED` sites.
    var_labels: BTreeMap<Rc<str>, BTreeSet<String>>,
    /// Edge variable → edge-label-name set seen at binding sites.
    edge_labels: BTreeMap<Rc<str>, BTreeSet<String>>,
    /// Reads deferred to scope end, when label facts are complete.
    pending: Vec<PendingRead>,
    /// Vertex variables whose physical properties are read by some downstream
    /// expression ([ADR 0089] §4 follow-up). Variables NOT in this set are
    /// topology-only consumers: they need only identity (element_id, traversal
    /// source, count, label predicates), not the full property map. The unconstrained
    /// scan that binds such a variable takes the marker + per-bucket path; scans
    /// that bind a property-reading variable stay tenancy-only.
    property_reading_vars: BTreeSet<Rc<str>>,
}

enum PendingRead {
    /// A vertex-property value read through `variable`.
    VertexProperty { variable: Rc<str>, property: String },
    /// An edge-inline-property read through the edge `variable`; covered by the edge
    /// label's traversal row once that label is known.
    EdgeProperty { variable: Rc<str> },
}

impl Scope {
    fn new(graph: GraphId) -> Self {
        Self {
            graph,
            var_labels: BTreeMap::new(),
            edge_labels: BTreeMap::new(),
            pending: Vec::new(),
            property_reading_vars: BTreeSet::new(),
        }
    }

    fn note_vertex_label(&mut self, variable: &str, label: &str) {
        self.var_labels
            .entry(Rc::from(variable))
            .or_default()
            .insert(label.to_owned());
    }

    fn note_edge_label(&mut self, variable: &str, label: &str) {
        self.edge_labels
            .entry(Rc::from(variable))
            .or_default()
            .insert(label.to_owned());
    }

    /// The single label name of a vertex variable, or `None` when unknown or ambiguous
    /// (both fail closed at resolution).
    fn unique_vertex_label(&self, variable: &str) -> Option<&String> {
        match self.var_labels.get(variable) {
            Some(labels) if labels.len() == 1 => labels.iter().next(),
            _ => None,
        }
    }

    fn unique_edge_label(&self, variable: &str) -> Option<&String> {
        match self.edge_labels.get(variable) {
            Some(labels) if labels.len() == 1 => labels.iter().next(),
            _ => None,
        }
    }

    fn defer_vertex_property(&mut self, variable: &str, property: &str) {
        // One (variable, property) read per scope, whatever plan sites mention it: an
        // index anchor defers its probe read AND keeps the residual equality filter, so
        // the same value read would otherwise extract identical duplicate demand rows.
        // Coverage semantics are unchanged — a repeated row adds nothing to satisfy.
        let already_pending = self.pending.iter().any(|read| {
            matches!(
                read,
                PendingRead::VertexProperty { variable: seen_variable, property: seen_property }
                    if seen_variable.as_ref() == variable && seen_property == property
            )
        });
        if already_pending {
            return;
        }
        self.pending.push(PendingRead::VertexProperty {
            variable: Rc::from(variable),
            property: property.to_owned(),
        });
    }

    fn defer_edge_property(&mut self, variable: &str) {
        self.pending.push(PendingRead::EdgeProperty {
            variable: Rc::from(variable),
        });
    }
}

/// Extract the complete requirement set of `plan` rooted at `root_graph`.
pub(crate) fn extract(
    plan: &PhysicalPlan,
    root_graph: GraphId,
    catalog: &dyn PlanCatalogView,
) -> RequirementSet {
    let mut reqs = RequirementSet::default();
    let mut scope = Scope::new(root_graph);
    scope.property_reading_vars = collect_property_reading_vars(plan);
    walk_ops(&plan.ops, &mut scope, &mut reqs, catalog);
    resolve_pending(&mut scope, &mut reqs, catalog);
    reqs
}

fn walk_ops(
    ops: &[PlanOp],
    scope: &mut Scope,
    reqs: &mut RequirementSet,
    catalog: &dyn PlanCatalogView,
) {
    for op in ops {
        walk_op(op, scope, reqs, catalog);
    }
}

/// Canonical grant rows demanded by traversing one declared edge label.
///
/// Mirrors grant lowering (`gql_grants::lower_privileges`): undirected or undeclared
/// labels carry only the unoriented row; declared directed labels carry directional rows.
fn traversal_rows(
    graph: u32,
    label_id: u32,
    undirected: Option<bool>,
    direction: EdgeDirection,
) -> RowGroup {
    let row = |direction: Option<Direction>| {
        edge_row(graph, GraphOperation::Traverse(direction), label_id)
    };
    match undirected {
        Some(true) | None => vec![row(None)],
        Some(false) => match direction_demand(direction) {
            DirectionDemand::Outgoing => vec![row(Some(Direction::Outgoing))],
            DirectionDemand::Incoming => vec![row(Some(Direction::Incoming))],
            DirectionDemand::Both => vec![
                row(Some(Direction::Outgoing)),
                row(Some(Direction::Incoming)),
            ],
        },
    }
}

/// Directional modifier requirement of one pattern direction. Undirected shapes over a
/// directed label require BOTH rows (ADR 0074 §2).
enum DirectionDemand {
    Outgoing,
    Incoming,
    Both,
}

fn direction_demand(direction: EdgeDirection) -> DirectionDemand {
    match direction {
        EdgeDirection::PointingRight => DirectionDemand::Outgoing,
        EdgeDirection::PointingLeft => DirectionDemand::Incoming,
        EdgeDirection::LeftOrRight
        | EdgeDirection::Undirected
        | EdgeDirection::LeftOrUndirected
        | EdgeDirection::UndirectedOrRight
        | EdgeDirection::AnyDirection => DirectionDemand::Both,
    }
}

/// Demand the traversal rows for one fixed edge-label reference.
fn require_edge_traversal_for_label(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: &EdgeLabelRef,
    direction: EdgeDirection,
) {
    let Some((id, undirected)) = catalog.edge_label(graph, &label.name) else {
        reqs.require_unattributed(graph);
        return;
    };
    reqs.require_all(
        graph,
        traversal_rows(graph.raw(), id, undirected, direction),
    );
}

/// Demand the traversal rows for a general edge label expression: OR-alternatives where
/// decomposable, tenancy-only otherwise (`NOT` / wildcard labels are not grantable).
fn require_edge_traversal_for_expr(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label_expr: &LabelExpr,
    direction: EdgeDirection,
) {
    match traversal_alternatives(label_expr, graph, catalog, direction) {
        Some(groups) => reqs.require_alternatives(graph, groups),
        None => reqs.require_unattributed(graph),
    }
}

/// Demand traversal rows for one hop given the plan's optional fixed label and optional
/// general label predicate. Neither being present is an unattributable wildcard traversal.
fn require_traversal(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: Option<&EdgeLabelRef>,
    label_expr: Option<&LabelExpr>,
    direction: EdgeDirection,
) {
    match (label, label_expr) {
        (Some(label), _) => {
            require_edge_traversal_for_label(reqs, graph, catalog, label, direction)
        }
        (None, Some(label_expr)) => {
            require_edge_traversal_for_expr(reqs, graph, catalog, label_expr, direction);
        }
        (None, None) => reqs.require_unattributed(graph),
    }
}

/// OR-alternative traversal groups for a label expression; `None` when the expression
/// cannot be decomposed into positive label names (`NOT`, `%`).
fn traversal_alternatives(
    expr: &LabelExpr,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    direction: EdgeDirection,
) -> Option<Vec<RowGroup>> {
    match expr {
        LabelExpr::Name(name) => {
            let (id, undirected) = catalog.edge_label(graph, name)?;
            Some(vec![traversal_rows(graph.raw(), id, undirected, direction)])
        }
        LabelExpr::Or(left, right) => {
            let mut left = traversal_alternatives(left, graph, catalog, direction)?;
            let right = traversal_alternatives(right, graph, catalog, direction)?;
            left.extend(right);
            Some(left)
        }
        LabelExpr::And(left, right) => {
            let left = traversal_alternatives(left, graph, catalog, direction)?;
            let right = traversal_alternatives(right, graph, catalog, direction)?;
            let mut combined = Vec::with_capacity(left.len() * right.len());
            for l in &left {
                for r in &right {
                    let mut group = l.clone();
                    group.extend(r.iter().cloned());
                    combined.push(group);
                }
            }
            Some(combined)
        }
        LabelExpr::Not(_) | LabelExpr::Wildcard => None,
    }
}

/// Demand the scan-side rows for a vertex scan: `MATCH`, plus value-read coverage for the
/// retained property map (`READ`) or each explicitly projected key (`READ_PROPERTY`).
fn require_vertex_scan_rows(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: Option<&NodeLabelRef>,
    property_projection: Option<&[Rc<str>]>,
    variable: &str,
    scope: &Scope,
) {
    let Some(label) = label else {
        // Unlabeled scans ([ADR 0089] §1–§4 follow-up): the topology-only shape binds
        // a vertex variable whose downstream output reads no physical properties (the
        // plan's output is `ElementId(a)`, `count(a)`, label predicates, or a
        // traversal source binding). The scan is pure topology and is restricted at
        // request build time to the caller's grantable vertex labels via the marker
        // demand. The walker must take this branch for any unlabeled scan whose
        // variable is not in `property_reading_vars` — the planner frequently leaves
        // `property_projection: None` even when no property is read, because traversal
        // sources keep the full property map. We do NOT trust the planner's
        // `property_projection` alone for the topology-only decision: we use the
        // downstream expression analysis ([`collect_property_reading_vars`]) as the
        // authoritative signal. Any other shape (property access, full vertex
        // hydration, or a labeled scan) keeps the atomic-reads contract: it stays
        // tenancy-only (fail closed) because property values are atomic.
        let variable_reads_properties = scope
            .property_reading_vars
            .contains::<str>(variable.as_ref());
        match property_projection {
            // Explicit empty projection: planner is telling us no property read.
            Some([]) => reqs.require_unconstrained_vertex_match(graph),
            // Explicit non-empty projection: planner is asking for these properties
            // to be read. Even if the downstream output doesn't reference them
            // (e.g., a no-op `RETURN` that omits the result), the executor must hydrate
            // them. Atomic.
            Some(_) => reqs.require_unattributed(graph),
            // Planner left the projection unspecified. Defer to the downstream
            // expression analysis: if no expression reads a physical property of
            // this variable, the scan is topology-only and takes the marker path.
            None if !variable_reads_properties => {
                reqs.require_unconstrained_vertex_match(graph);
            }
            None => reqs.require_unattributed(graph),
        }
        return;
    };
    let Some(label_id) = catalog.vertex_label(graph, &label.name) else {
        reqs.require_unattributed(graph);
        return;
    };
    let graph_raw = graph.raw();
    reqs.require(
        graph,
        vertex_row(graph_raw, GraphOperation::Match, label_id),
    );
    // Property read demand ([ADR 0089] §4 follow-up): the planner's
    // `property_projection` is the authoritative signal for a labeled scan. A bare
    // `Variable(v)` reference in the output (vertex hydration, `RETURN v`) hydrates
    // the full property map, so the planner sets `property_projection: None` and we
    // demand READ. A `property_projection: Some([...])` demands READ_PROPERTY for
    // every listed key. A `property_projection: Some([])` (empty list) is the
    // planner's explicit "no property read" signal — the scan is topology-only and
    // MATCH alone covers it. This keeps the explorer's edge-load shape working
    // for a MATCH-only caller: specialization rewrites the unconstrained endpoint
    // into `NodeScan { label: Some(L) }` whose `property_projection` the planner
    // already set to `Some([])` because the output reads no physical property of
    // the endpoint variable.
    match property_projection {
        None => reqs.require(graph, vertex_row(graph_raw, GraphOperation::Read, label_id)),
        Some([]) => {}
        Some(projected) => {
            for name in projected {
                let Some(property_id) = catalog.property(graph, name) else {
                    reqs.require_unattributed(graph);
                    continue;
                };
                reqs.require(graph, vertex_property_row(graph_raw, label_id, property_id));
            }
        }
    }
}

/// Demand the scan-side rows for a vertex search (ADR 0078 §1 layer 1): `MATCH` on every
/// label the vector index spans plus value-read coverage of the projected embedding source
/// property per label. The label rows are conjunctive — per-candidate visibility has no
/// row-level grant filter, so a caller must be able to read the whole spanned label set,
/// not just one of its labels. Label names are recorded as scope facts for the searched
/// binding so downstream deferred reads attribute like an equivalent labeled scan; a
/// multi-label span leaves those reads ambiguous (fail closed), and an unknown index name
/// is unattributed.
fn require_vector_search_rows(
    reqs: &mut RequirementSet,
    scope: &mut Scope,
    catalog: &dyn PlanCatalogView,
    binding: &str,
    index_name: &str,
) {
    let Some(source) = catalog.vector_index_source(scope.graph, index_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let graph_raw = scope.graph.raw();
    for (label_id, label_name) in &source.labels {
        scope.note_vertex_label(binding, label_name);
        reqs.require(
            scope.graph,
            vertex_row(graph_raw, GraphOperation::Match, *label_id),
        );
        if let Some(property_id) = source.embedding_property_id {
            reqs.require(
                scope.graph,
                vertex_property_row(graph_raw, *label_id, property_id),
            );
        }
    }
}

/// Demand one mutation row on a labeled vertex; open-schema names that stopped resolving
/// degrade to tenancy-only coverage.
fn require_vertex_mutation(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: &NodeLabelRef,
    operation: GraphOperation,
) {
    let Some(label_id) = catalog.vertex_label(graph, &label.name) else {
        reqs.require_unattributed(graph);
        return;
    };
    reqs.require(graph, vertex_row(graph.raw(), operation, label_id));
}

/// `CREATE` rows for freshly inserted vertices.
fn require_create_labels(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    labels: &[NodeLabelRef],
) {
    for label in labels {
        require_vertex_mutation(reqs, graph, catalog, label, GraphOperation::Create);
    }
}

/// Attribute a mutation on a bound variable to its single known vertex label, falling back
/// to tenancy-only when the variable is unlabeled or multi-labeled.
fn require_mutation_on_variable(
    reqs: &mut RequirementSet,
    scope: &Scope,
    catalog: &dyn PlanCatalogView,
    variable: &str,
    operation: GraphOperation,
) {
    let Some(label_name) = scope.unique_vertex_label(variable) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some(label_id) = catalog.vertex_label(scope.graph, label_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    reqs.require(
        scope.graph,
        vertex_row(scope.graph.raw(), operation, label_id),
    );
}

/// Attribute a deletion of an edge variable to its single known edge label.
fn require_edge_deletion_on_variable(
    reqs: &mut RequirementSet,
    scope: &Scope,
    catalog: &dyn PlanCatalogView,
    variable: &str,
) {
    let Some(label_name) = scope.unique_edge_label(variable) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some((id, _)) = catalog.edge_label(scope.graph, label_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    reqs.require(
        scope.graph,
        edge_row(scope.graph.raw(), GraphOperation::Delete, id),
    );
}

/// Record an expand hop's destination binding and deferred destination projections.
fn note_expand_dst(scope: &mut Scope, dst: &str, dst_property_projection: Option<&[Rc<str>]>) {
    for name in dst_property_projection.iter().flat_map(|p| p.iter()) {
        scope.defer_vertex_property(dst, name);
    }
}

/// Note the traversed edge variable's label when the executor binds it.
fn note_bound_edge(
    scope: &mut Scope,
    edge: &str,
    label: Option<&EdgeLabelRef>,
    emit_edge_binding: bool,
) {
    if emit_edge_binding && let Some(label) = label {
        scope.note_edge_label(edge, &label.name);
    }
}

/// The one label name shared by every resolved index-bound label, or `None` when the
/// inputs are empty, any input fails to resolve, or the resolutions disagree — every
/// ambiguous shape fails closed instead of attributing reads to a guessed label.
fn unique_index_bound_label(resolved: impl Iterator<Item = Option<String>>) -> Option<String> {
    let mut unique: Option<String> = None;
    for label in resolved {
        let label = label?;
        match &unique {
            Some(seen) if *seen == label => {}
            Some(_) => return None,
            None => unique = Some(label),
        }
    }
    unique
}

fn walk_op(
    op: &PlanOp,
    scope: &mut Scope,
    reqs: &mut RequirementSet,
    catalog: &dyn PlanCatalogView,
) {
    let graph = scope.graph;
    match op {
        // ──── Scan ────
        PlanOp::NodeScan {
            variable,
            label,
            property_projection,
        } => {
            require_vertex_scan_rows(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                property_projection.as_deref(),
                variable,
                scope,
            );
            if let Some(label) = label {
                scope.note_vertex_label(variable, &label.name);
            }
        }

        PlanOp::IndexScan {
            variable,
            property,
            property_projection,
            ..
        } => {
            // The equality/range probe reads one property value of the scanned variable;
            // its reads stay deferred and resolve at scope end. The scan itself binds only
            // vertices of the indexed property's bound label, so a catalog-provable unique
            // active vertex index attributes the anchor exactly like a labeled NodeScan
            // (same demand rows). An ambiguous or missing index leaves no label fact and
            // the deferred reads fail closed at scope resolution.
            if let Some(label_name) = catalog
                .property(graph, property)
                .and_then(|property_id| catalog.vertex_property_index_label(graph, property_id))
            {
                scope.note_vertex_label(variable, &label_name);
                require_vertex_scan_rows(
                    reqs,
                    graph,
                    catalog,
                    Some(&NodeLabelRef::new(label_name.as_str())),
                    property_projection.as_deref(),
                    variable,
                    scope,
                );
            }
            scope.defer_vertex_property(variable, property);
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_vertex_property(variable, name);
            }
        }

        PlanOp::TextScan {
            variable,
            label,
            property,
            ..
        } => {
            // The text scan binds labeled vertices and reads the indexed TEXT property;
            // grant coverage on the label plus a deferred property requirement mirror
            // NodeScan + IndexScan above.
            scope.note_vertex_label(variable, &label.name);
            scope.defer_vertex_property(variable, property);
        }

        PlanOp::EdgeIndexScan {
            variable,
            property,
            property_projection,
            ..
        } => {
            scope.defer_edge_property(property);
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_edge_property(name);
            }
            let _ = variable;
        }

        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            direction,
            label,
            near_property_projection,
            far_property_projection,
            ..
        } => {
            require_traversal(reqs, graph, catalog, label.as_ref(), None, *direction);
            if let Some(label) = label {
                scope.note_edge_label(edge, &label.name);
            }
            for (variable, projection) in [
                (near, near_property_projection),
                (far, far_property_projection),
            ] {
                for name in projection.iter().flat_map(|p| p.iter()) {
                    scope.defer_vertex_property(variable, name);
                }
            }
        }

        PlanOp::ConditionalIndexScan {
            candidates,
            fallback_label,
            fallback_variable,
            property_projection,
        } => {
            for candidate in candidates {
                scope.defer_vertex_property(&candidate.variable, &candidate.property);
            }
            // Every candidate binds the variable the fallback scan binds, so a consistent
            // index-derived resolution attributes the deferred reads like a labeled scan:
            // all candidate properties must resolve to ONE uniquely-indexed label that
            // agrees with the fallback label when one exists. Any disagreement — or a
            // candidate property with no unique active index — is catalog ambiguity the
            // plan never resolved, so no label fact is noted (fail closed).
            let agreed_label = unique_index_bound_label(candidates.iter().map(|candidate| {
                catalog
                    .property(graph, &candidate.property)
                    .and_then(|property_id| catalog.vertex_property_index_label(graph, property_id))
            }));
            match (agreed_label, fallback_label) {
                (Some(label), None) => {
                    scope.note_vertex_label(fallback_variable, &label);
                    for candidate in candidates {
                        scope.note_vertex_label(&candidate.variable, &label);
                    }
                }
                (Some(label), Some(fallback)) if fallback.name.as_ref() == label => {
                    scope.note_vertex_label(fallback_variable, &label);
                    for candidate in candidates {
                        scope.note_vertex_label(&candidate.variable, &label);
                    }
                }
                // No index-derived fact: only a candidate-free conditional scan keeps the
                // pre-existing fallback-label attribution (nothing contradicts it).
                (None, Some(fallback)) if candidates.is_empty() => {
                    scope.note_vertex_label(fallback_variable, &fallback.name);
                }
                _ => {}
            }
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_vertex_property(fallback_variable, name);
            }
        }

        // ──── Filter ────
        PlanOp::PropertyFilter { predicates, .. } => {
            for predicate in predicates {
                collect_expr_reads(predicate, scope);
            }
        }

        // ──── Traversal ────
        PlanOp::Expand {
            dst,
            edge,
            direction,
            label,
            label_expr,
            edge_property_projection,
            dst_property_projection,
            emit_edge_binding,
            ..
        } => {
            require_traversal(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                label_expr.as_ref(),
                *direction,
            );
            note_bound_edge(scope, edge, label.as_ref(), *emit_edge_binding);
            for name in edge_property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_edge_property(edge);
                let _ = name;
            }
            note_expand_dst(scope, dst, dst_property_projection.as_deref());
        }

        PlanOp::ExpandFilter {
            dst,
            edge,
            direction,
            label,
            label_expr,
            dst_filter,
            edge_property_projection,
            dst_property_projection,
            emit_edge_binding,
            ..
        } => {
            require_traversal(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                label_expr.as_ref(),
                *direction,
            );
            note_bound_edge(scope, edge, label.as_ref(), *emit_edge_binding);
            for name in edge_property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_edge_property(edge);
                let _ = name;
            }
            for predicate in dst_filter {
                collect_expr_reads(predicate, scope);
            }
            note_expand_dst(scope, dst, dst_property_projection.as_deref());
        }

        PlanOp::ShortestPath {
            edge,
            emit_edge_binding,
            direction,
            label,
            label_expr,
            cost,
            ..
        } => {
            require_traversal(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                label_expr.as_ref(),
                *direction,
            );
            note_bound_edge(scope, edge, label.as_ref(), *emit_edge_binding);
            if let ShortestPathCost::EdgeCostExpr { expr, .. } = cost {
                collect_expr_reads(expr, scope);
            }
        }

        // ──── GQL-specific ────
        PlanOp::Let { bindings } => {
            for binding in bindings {
                collect_expr_reads(&binding.value, scope);
            }
        }

        PlanOp::For { list, .. } => collect_expr_reads(list, scope),

        PlanOp::Filter { condition } => collect_expr_reads(condition, scope),

        PlanOp::Search {
            binding, provider, ..
        } => {
            collect_expr_reads(provider.query(), scope);
            collect_expr_reads(provider.limit(), scope);
            if let Some(filter) = provider.filter() {
                collect_expr_reads(filter, scope);
            }
            // ADR 0078 §1 layer 1: the search itself demands the same rows as a scan of
            // its source labels (plus the projected embedding source property), so
            // unauthorized callers are rejected before any ANN dispatch.
            let SearchProviderPlan::VectorIndex { index_name, .. } = provider;
            let index_name = index_name
                .iter()
                .map(|part| part.as_ref())
                .collect::<Vec<_>>()
                .join(".");
            require_vector_search_rows(reqs, scope, catalog, binding.as_ref(), &index_name);
        }

        // ──── Procedure calls ────
        PlanOp::CallProcedure { .. } => {
            // Named procedures stay behind the CALL_PROCEDURE capability gate upstream
            // (ADR 0074 §1b); no data-plane resource exists for them yet.
        }

        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            walk_ops(&sub_plan.ops, scope, reqs, catalog);
        }

        PlanOp::UseGraph {
            graph_name,
            sub_plan,
        } => {
            let Some(sub_plan) = sub_plan else { return };
            match use_graph_target(graph_name, scope.graph, catalog) {
                Some(target) => {
                    // Fresh scope: another graph's catalogs own this segment's names, so
                    // outer label facts must not leak into its attribution.
                    let mut child = Scope::new(target);
                    walk_ops(sub_plan, &mut child, reqs, catalog);
                    resolve_pending(&mut child, reqs, catalog);
                }
                None => reqs.root_unattributed = true,
            }
        }

        // ──── Join operators ────
        PlanOp::HashJoin { left, right, .. } => {
            walk_ops(left, scope, reqs, catalog);
            walk_ops(right, scope, reqs, catalog);
        }

        PlanOp::CartesianProduct { left, right } => {
            walk_ops(left, scope, reqs, catalog);
            walk_ops(right, scope, reqs, catalog);
        }

        // ──── Aggregation / Output ────
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for expr in group_by {
                collect_expr_reads(expr, scope);
            }
            for spec in aggregates {
                for expr in [&spec.expr, &spec.expr2, &spec.filter]
                    .into_iter()
                    .flatten()
                {
                    collect_expr_reads(expr, scope);
                }
                if let Some(order_by) = &spec.order_by {
                    for item in &order_by.items {
                        collect_expr_reads(&item.expr, scope);
                    }
                }
            }
        }

        PlanOp::Project { columns, .. } => {
            for column in columns {
                collect_expr_reads(&column.expr, scope);
            }
        }

        PlanOp::Sort { order_by } => {
            for item in &order_by.items {
                collect_expr_reads(&item.expr, scope);
            }
        }

        PlanOp::Limit { count, offset } => {
            for expr in [count, offset].into_iter().flatten() {
                collect_expr_reads(expr, scope);
            }
        }

        PlanOp::SetOperation { right, .. } => walk_ops(&right.ops, scope, reqs, catalog),

        PlanOp::OptionalMatch { sub_plan } => walk_ops(sub_plan, scope, reqs, catalog),

        // [ADR 0082] §5 invariants 1–2: the chain is policy machinery lowered strictly
        // after enforcement (gql.rs ordering), so its internal traversals and terminal
        // reads add no requirements and project nothing. The walker stays blind to it —
        // "the relationship is the gate", not the caller's own privilege set.
        PlanOp::SemiApply { .. } => {}

        PlanOp::IndexIntersection {
            variable,
            scans,
            property_projection,
        } => {
            for spec in scans {
                scope.defer_vertex_property(variable, &spec.property);
            }
            // Intersected index scans all bind the same variable, so the deferred reads
            // attribute like a labeled scan only when EVERY scanned property resolves to
            // the same uniquely-indexed label; any disagreement or unresolvable property
            // leaves no label fact (fail closed).
            if let Some(label) = unique_index_bound_label(scans.iter().map(|spec| {
                catalog
                    .property(graph, &spec.property)
                    .and_then(|property_id| catalog.vertex_property_index_label(graph, property_id))
            })) {
                scope.note_vertex_label(variable, &label);
            }
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_vertex_property(variable, name);
            }
        }

        PlanOp::WorstCaseOptimalJoin { edges, .. } => {
            for wcoj in edges {
                require_traversal(
                    reqs,
                    graph,
                    catalog,
                    wcoj.label.as_ref(),
                    wcoj.label_expr.as_ref(),
                    wcoj.direction,
                );
                note_bound_edge(
                    scope,
                    &wcoj.variable,
                    wcoj.label.as_ref(),
                    /* hop edges are always bound */ true,
                );
                for predicate in &wcoj.dst_filter {
                    collect_expr_reads(predicate, scope);
                }
                note_expand_dst(scope, &wcoj.dst, None);
            }
        }

        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => {
            for item in &order_by.items {
                collect_expr_reads(&item.expr, scope);
            }
            collect_expr_reads(k, scope);
            if let Some(offset) = offset {
                collect_expr_reads(offset, scope);
            }
        }

        // ──── Pipeline boundaries ────
        PlanOp::Materialize { columns, .. } => {
            for column in columns {
                collect_expr_reads(&column.expr, scope);
            }
        }

        // ──── DML ────
        PlanOp::InsertVertex {
            labels, properties, ..
        } => {
            require_create_labels(reqs, graph, catalog, labels);
            for assignment in properties {
                collect_expr_reads(&assignment.value, scope);
            }
        }

        PlanOp::InsertEdge {
            direction,
            labels,
            properties,
            ..
        } => {
            for label in labels {
                let Some((id, _)) = catalog.edge_label(graph, &label.name) else {
                    reqs.require_unattributed(graph);
                    continue;
                };
                reqs.require(graph, edge_row(graph.raw(), GraphOperation::Create, id));
            }
            let _ = direction; // insertion orientation carries no additional privilege
            for assignment in properties {
                collect_expr_reads(&assignment.value, scope);
            }
        }

        PlanOp::SetProperties { items } => {
            for item in items {
                match item {
                    SetPlanItem::Property {
                        variable, value, ..
                    } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        collect_expr_reads(value, scope);
                    }
                    SetPlanItem::AllProperties { variable, value } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        collect_expr_reads(value, scope);
                    }
                    SetPlanItem::Label { variable, label } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        // Adding a label mutates membership in the added vocabulary too.
                        require_vertex_mutation(
                            reqs,
                            graph,
                            catalog,
                            label,
                            GraphOperation::Update,
                        );
                    }
                }
            }
        }

        PlanOp::RemoveProperties { items } => {
            for item in items {
                match item {
                    RemovePlanItem::Property { variable, .. } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                    }
                    RemovePlanItem::Label { variable, label } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        require_vertex_mutation(
                            reqs,
                            graph,
                            catalog,
                            label,
                            GraphOperation::Update,
                        );
                    }
                }
            }
        }

        PlanOp::DeleteVertex { variable } => {
            require_mutation_on_variable(reqs, scope, catalog, variable, GraphOperation::Delete);
        }

        PlanOp::DetachDeleteVertex { variable } => {
            require_mutation_on_variable(reqs, scope, catalog, variable, GraphOperation::Delete);
            // Detach also deletes incident edges whose labels the plan never enumerates.
            reqs.require_unattributed(graph);
        }

        PlanOp::DeleteEdge { variable } => {
            require_edge_deletion_on_variable(reqs, scope, catalog, variable);
        }
    }
}

/// Resolve a `USE GRAPH` segment target; `None` marks the segment unattributable.
fn use_graph_target(
    graph_name: &[Rc<str>],
    current: GraphId,
    catalog: &dyn PlanCatalogView,
) -> Option<GraphId> {
    let [name] = graph_name else {
        return None;
    };
    if name.as_ref() == "CURRENT_GRAPH" {
        return Some(current);
    }
    catalog.graph_by_name(name)
}

/// Collect property-value reads and `IS LABELED` label facts from one expression tree.
fn collect_expr_reads(expr: &Expr, scope: &mut Scope) {
    match &expr.kind {
        ExprKind::PropertyAccess {
            expr: base,
            property,
        } => {
            // Only property accesses whose immediate base is a graph variable read graph
            // properties; nested suffixes address fields of returned records.
            if let ExprKind::Variable(name) = &base.kind {
                scope.defer_vertex_property(name, property);
            }
        }
        ExprKind::IsLabeled {
            expr: base, label, ..
        } => {
            if let ExprKind::Variable(name) = &base.kind {
                let mut names = Vec::new();
                let mut decidable = true;
                collect_label_names(label, &mut names, &mut decidable);
                if decidable {
                    for label_name in names {
                        scope.note_vertex_label(name, &label_name);
                    }
                }
            }
        }
        _ => {}
    }
    for_each_immediate_child_expr(expr, |child| collect_expr_reads(child, scope));
}

/// Positive label names of a label expression; `decidable` stays true only when the whole
/// expression is built from `Name` / `And` / `Or`.
fn collect_label_names(expr: &LabelExpr, names: &mut Vec<String>, decidable: &mut bool) {
    match expr {
        LabelExpr::Name(name) => names.push(name.clone()),
        LabelExpr::And(left, right) | LabelExpr::Or(left, right) => {
            collect_label_names(left, names, decidable);
            collect_label_names(right, names, decidable);
        }
        LabelExpr::Not(_) | LabelExpr::Wildcard => *decidable = false,
    }
}

/// Resolve deferred reads once a scope's label facts are complete.
fn resolve_pending(scope: &mut Scope, reqs: &mut RequirementSet, catalog: &dyn PlanCatalogView) {
    let pending = std::mem::take(&mut scope.pending);
    for read in pending {
        match read {
            PendingRead::VertexProperty { variable, property } => {
                resolve_vertex_property_read(scope, reqs, catalog, &variable, &property);
            }
            PendingRead::EdgeProperty { variable } => {
                // An attributed edge-property read is covered by the edge label's
                // traversal row (Phase 1 has no separate edge-property resource); an
                // unattributable one degrades to tenancy-only coverage.
                if scope.unique_edge_label(&variable).is_none() {
                    reqs.require_unattributed(scope.graph);
                }
            }
        }
    }
}

fn resolve_vertex_property_read(
    scope: &Scope,
    reqs: &mut RequirementSet,
    catalog: &dyn PlanCatalogView,
    variable: &str,
    property: &str,
) {
    let Some(label_name) = scope.unique_vertex_label(variable) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some(label_id) = catalog.vertex_label(scope.graph, label_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some(property_id) = catalog.property(scope.graph, property) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    reqs.require(
        scope.graph,
        vertex_property_row(scope.graph.raw(), label_id, property_id),
    );
}

// ──── Evaluation ────

/// Coverage probe over the caller's effective privileges (`caller ∪ PUBLIC`,
/// expiry-aware). Deliberately exposes no administrative-capability input.
pub(crate) trait EffectiveGrants {
    fn covers(&self, caller: &Principal, privilege: &Privilege) -> bool;

    /// Whether `caller` (or `PUBLIC`) holds at least one unexpired vertex-label `MATCH`
    /// grant on `graph`, including the wildcard `NODES *` row ([ADR 0089] §2). Backs
    /// the unconstrained-scan marker demand.
    fn holds_any_vertex_label_match(&self, caller: &Principal, graph: u32) -> bool;
}

/// Implicit-root coverage (ADR 0074 §3 invariant 3): the registry owner's (and admins')
/// authority over a graph is evaluated at enforcement time from the registry and is never
/// materialized as grant rows.
pub(crate) trait GraphTenancy {
    fn is_tenant(&self, graph_raw: u32, caller: &Principal) -> bool;
}

/// Whether `reqs` is fully covered for `caller`. Uniform denial: the caller learns only
/// that the query may not run, never which demand failed.
pub(crate) fn authorize_requirements(
    reqs: &RequirementSet,
    root_graph_raw: u32,
    caller: &Principal,
    grants: &dyn EffectiveGrants,
    tenancy: &dyn GraphTenancy,
) -> bool {
    if reqs.root_unattributed && !tenancy.is_tenant(root_graph_raw, caller) {
        return false;
    }
    for (graph_raw, demands) in &reqs.graphs {
        if tenancy.is_tenant(*graph_raw, caller) {
            continue;
        }
        if demands.unattributed {
            return false;
        }
        // Unconstrained-vertex-match marker ([ADR 0089] §2): the caller must hold `MATCH`
        // on at least one vertex label (or the wildcard `NODES *` row) on this graph.
        // The per-bucket restriction is applied at request build time.
        if demands.unconstrained_vertex_match
            && !grants.holds_any_vertex_label_match(caller, *graph_raw)
        {
            return false;
        }
        if !demands
            .conjunctive
            .iter()
            .all(|privilege| grants.covers(caller, privilege))
        {
            return false;
        }
        // Alternation groups: any ONE fully covered group satisfies the demand; no
        // groups means no alternation demand.
        if !demands.alternatives.is_empty()
            && !demands.alternatives.iter().any(|group| {
                group
                    .iter()
                    .all(|privilege| grants.covers(caller, privilege))
            })
        {
            return false;
        }
    }
    true
}

/// Stored-grant coverage probe (`ROUTER_AUTH_GRANTS`, expiry-aware).
struct StoredGrants;

impl EffectiveGrants for StoredGrants {
    fn covers(&self, caller: &Principal, privilege: &Privilege) -> bool {
        let now_ns = crate::facade::store::ic_time_ns();
        let exact = crate::facade::stable::ROUTER_AUTH_GRANTS.with_borrow(|grants| {
            grants.holds(GrantSubject::effective_for(caller), privilege, now_ns)
                || grants.holds(GrantSubject::Public, privilege, now_ns)
        });
        if exact {
            return true;
        }
        // Wildcard `NODES *` ([ADR 0089] §5): a `Match` on a concrete `VertexLabel`
        // resource is also covered by the `AllVertexLabels` row for the same graph.
        if let Privilege::Graph(GraphPrivilege {
            graph: priv_graph,
            operation: GraphOperation::Match,
            resource: GraphResource::VertexLabel(_),
        }) = privilege
        {
            let wildcard = Privilege::Graph(GraphPrivilege {
                graph: *priv_graph,
                operation: GraphOperation::Match,
                resource: GraphResource::AllVertexLabels,
            });
            crate::facade::stable::ROUTER_AUTH_GRANTS.with_borrow(|grants| {
                grants.holds(GrantSubject::effective_for(caller), &wildcard, now_ns)
                    || grants.holds(GrantSubject::Public, &wildcard, now_ns)
            })
        } else {
            false
        }
    }

    fn holds_any_vertex_label_match(&self, caller: &Principal, graph: u32) -> bool {
        crate::facade::auth::holds_any_vertex_label_match(graph, caller)
    }
}

/// Registry-backed ownership probe.
struct StoreTenancy<'a> {
    store: &'a RouterStore,
}

impl GraphTenancy for StoreTenancy<'_> {
    fn is_tenant(&self, graph_raw: u32, caller: &Principal) -> bool {
        self.store
            .is_graph_tenant(GraphId::from_raw(graph_raw), *caller)
    }
}

/// Whether `reqs` is covered for `caller` against live Router state (`caller ∪ PUBLIC`
/// grant rows plus implicit ownership root). The single seam every enforcement surface
/// uses, so evaluation semantics cannot drift between call sites.
pub(crate) fn requirements_cover(
    reqs: &RequirementSet,
    root_graph_raw: u32,
    caller: &Principal,
    store: &RouterStore,
) -> bool {
    authorize_requirements(
        reqs,
        root_graph_raw,
        caller,
        &StoredGrants,
        &StoreTenancy { store },
    )
}

/// Extract the requirement set of `plan` against live Router catalogs.
///
/// Used by ad-hoc enforcement and by prepared registration, where the extracted set is
/// stored on the record (ADR 0074 slice 3).
pub(crate) fn extract_live(
    store: &RouterStore,
    plan: &PhysicalPlan,
    root_graph: GraphId,
) -> RequirementSet {
    extract(plan, root_graph, &RouterCatalogView { store })
}

/// Enforce plan-time data-plane authorization for `plan` executing on `root_graph`.
///
/// Single failure contract (ADR 0074 §4): a uniform [`RouterError::Forbidden`] that names
/// neither the missing privilege nor the resource.
pub(crate) fn enforce_data_plane_authorization(
    store: &RouterStore,
    caller: &Principal,
    root_graph: GraphId,
    plan: &PhysicalPlan,
) -> Result<(), RouterError> {
    let reqs = extract_live(store, plan, root_graph);
    if requirements_cover(&reqs, root_graph.raw(), caller, store) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Enforce prepared-query data-plane authorization (ADR 0074 slice 3).
///
/// The registration-time [`RequirementSet`] stored on the record is the primary checked
/// artifact. When it denies, the live walk of the actually-dispatched `plan` decides as
/// the fallback: catalogs may have drifted since registration, and derived sorted plans
/// carry demands the stored base-plan set does not describe. Both evaluations fail closed
/// and share one semantics ([`requirements_cover`]); admitting only when either admits is
/// safe because catalog ids are monotonic (ADR 0074 invariant 4), so a stored demand can
/// never re-bind to different vocabulary.
///
/// **Stored-set staleness is fail-closed by design.** When vocabulary named by a stored
/// requirement set is dropped (today: whole-graph teardown via
/// `purge_graph_vocabulary_partitions`, which also sweeps the graph's grant rows), the
/// dead-id demands can never be covered again — grants resolve through live catalogs whose
/// ids are monotonic, so no future grant row can match a dropped id. Stored requirement
/// sets are therefore *not* recomputed or patched in place; queries referencing dropped
/// vocabulary stay uniformly denied until the prepared query is re-registered against the
/// current catalogs (re-registration replaces the whole record).
pub(crate) fn enforce_prepared_data_plane_authorization(
    store: &RouterStore,
    caller: &Principal,
    root_graph: GraphId,
    stored: &RequirementSet,
    plan: &PhysicalPlan,
) -> Result<(), RouterError> {
    let stored_covers = requirements_cover(stored, root_graph.raw(), caller, store);
    if stored_covers {
        return Ok(());
    }
    enforce_data_plane_authorization(store, caller, root_graph, plan)
}

// ──── EXPLAIN AUTHORIZATION report mode (ADR 0084) ────

/// Coverage source of one demanded row in report mode ([ADR 0084] §5).
///
/// The variant set is closed by construction: only the explained principal's own rows,
/// `PUBLIC` rows, or the ownership root can ever cover a demand, so self-mode rendering
/// can never name a foreign principal.
///
/// [ADR 0084]: https://github.com/gleaph/gleaph/blob/main/design/adr/0084-explain-authorization-diagnosis.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoverageSource {
    /// Implicit ownership root (registry tenancy).
    Tenancy,
    /// A stored row held by the explained principal.
    OwnGrant,
    /// A stored `PUBLIC` row.
    PublicGrant,
}

/// One rendered verdict row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoverageRow {
    /// Grant-grammar-shaped description of the demanded privilege.
    pub requirement: String,
    /// Covering source; `None` when uncovered.
    pub source: Option<CoverageSource>,
}

/// One graph's extracted demands with per-row coverage attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GraphCoverage {
    pub graph_raw: u32,
    /// Display name resolved at build time (rendering stays catalog-free).
    pub graph_name: String,
    pub conjunctive: Vec<CoverageRow>,
    /// Alternation groups rendered as "any-of"; each group lists its arms' sources.
    pub alternatives: Vec<Vec<CoverageRow>>,
    /// Unattributed residue: only the ownership root can cover it.
    pub unattributed: bool,
}

/// The join between one requirement set and one principal's coverage (report mode).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoverageReport {
    pub covered: bool,
    pub root_graph_raw: u32,
    pub graphs: Vec<GraphCoverage>,
    /// Set when a demand could not even be pinned to a graph; covered by ownership on
    /// the executing root only.
    pub root_unattributed: bool,
}

/// Split source probes over the same stored rows [`StoredGrants`] reads.
///
/// The disjunction of both arms IS the enforcement probe (`EffectiveGrants::covers`
/// evaluates exactly `holds(effective_for(caller)) || holds(Public)`), so attributing a
/// source cannot shift evaluation semantics — proven by an agreement test pairing this
/// walk with `authorize_requirements` on identical fixtures.
pub(crate) trait GrantSourceProbes {
    fn holds_own_row(&self, subject: &Principal, privilege: &Privilege) -> bool;
    fn holds_public_row(&self, privilege: &Privilege) -> bool;
}

/// Production probes over `ROUTER_AUTH_GRANTS` (expiry-aware): the two halves of
/// [`StoredGrants::covers`].
struct StoredGrantSources;

impl GrantSourceProbes for StoredGrantSources {
    fn holds_own_row(&self, subject: &Principal, privilege: &Privilege) -> bool {
        let now_ns = crate::facade::store::ic_time_ns();
        crate::facade::stable::ROUTER_AUTH_GRANTS.with_borrow(|grants| {
            grants.holds(GrantSubject::effective_for(subject), privilege, now_ns)
        })
    }

    fn holds_public_row(&self, privilege: &Privilege) -> bool {
        let now_ns = crate::facade::store::ic_time_ns();
        crate::facade::stable::ROUTER_AUTH_GRANTS
            .with_borrow(|grants| grants.holds(GrantSubject::Public, privilege, now_ns))
    }
}

/// Catalog-name resolution for requirement rendering; ids alone mean nothing to
/// operators. Production resolves through the graph-scoped catalogs.
pub(crate) trait RequirementNames {
    fn graph_name(&self, graph_raw: u32) -> Option<String>;
    fn vertex_label(&self, graph_raw: u32, label_id: u32) -> Option<String>;
    fn edge_label(&self, graph_raw: u32, label_id: u32) -> Option<String>;
    fn property(&self, graph_raw: u32, property_id: u32) -> Option<String>;
}

/// Production name resolution over the Router graph-scoped catalogs.
struct StoreRequirementNames<'a> {
    store: &'a RouterStore,
}

impl RequirementNames for StoreRequirementNames<'_> {
    fn graph_name(&self, graph_raw: u32) -> Option<String> {
        crate::facade::stable::graph_catalog::graph_name(GraphId::from_raw(graph_raw))
    }

    fn vertex_label(&self, graph_raw: u32, label_id: u32) -> Option<String> {
        let label_id = u16::try_from(label_id).ok()?;
        self.store
            .reverse_vertex_label_name(
                GraphId::from_raw(graph_raw),
                VertexLabelId::from_raw(label_id),
            )
            .ok()
    }

    fn edge_label(&self, graph_raw: u32, label_id: u32) -> Option<String> {
        let label_id = u16::try_from(label_id).ok()?;
        self.store
            .reverse_edge_label_name(
                GraphId::from_raw(graph_raw),
                EdgeLabelId::from_raw(label_id),
            )
            .ok()
    }

    fn property(&self, graph_raw: u32, property_id: u32) -> Option<String> {
        self.store
            .reverse_property_name(
                GraphId::from_raw(graph_raw),
                PropertyId::from_raw(property_id),
            )
            .ok()
    }
}

/// Grant-grammar-shaped text of one demanded operation half.
fn describe_operation(operation: GraphOperation) -> &'static str {
    match operation {
        GraphOperation::Match => "MATCH",
        GraphOperation::Traverse(Some(Direction::Outgoing)) => "TRAVERSE OUTGOING",
        GraphOperation::Traverse(Some(Direction::Incoming)) => "TRAVERSE INCOMING",
        GraphOperation::Traverse(None) => "TRAVERSE",
        GraphOperation::Read | GraphOperation::ReadProperty => "READ",
        GraphOperation::Create => "CREATE",
        GraphOperation::Update => "UPDATE",
        GraphOperation::Delete => "DELETE",
    }
}

/// Grant-grammar-shaped text of one demanded resource half. Unresolvable ids render as
/// `#<id>` rather than being dropped: the report never hides a demand.
fn describe_resource(
    graph_raw: u32,
    resource: &GraphResource,
    names: &dyn RequirementNames,
) -> String {
    match *resource {
        GraphResource::VertexLabel(label) => format!(
            "NODES {}",
            names
                .vertex_label(graph_raw, label)
                .unwrap_or_else(|| format!("#{label}"))
        ),
        GraphResource::EdgeLabel(label) => format!(
            "EDGES {}",
            names
                .edge_label(graph_raw, label)
                .unwrap_or_else(|| format!("#{label}"))
        ),
        GraphResource::VertexProperty { label, property } => format!(
            "NODES {} {{ {} }}",
            names
                .vertex_label(graph_raw, label)
                .unwrap_or_else(|| format!("#{label}")),
            names
                .property(graph_raw, property)
                .unwrap_or_else(|| format!("#{property}"))
        ),
        GraphResource::AllVertexLabels => "NODES *".to_owned(),
    }
}

/// Grant-grammar-shaped text of one demanded row ([ADR 0084] §4); metadata-plane rows
/// render with their scope so elevation demands stay visible in reports. Prepared-execute
/// rows never occur in extracted requirement sets, but the match stays exhaustive.
fn describe_privilege(privilege: &Privilege, names: &dyn RequirementNames) -> String {
    match privilege {
        Privilege::Graph(graph_privilege) => format!(
            "{} ON {}",
            describe_operation(graph_privilege.operation),
            describe_resource(graph_privilege.graph, &graph_privilege.resource, names)
        ),
        Privilege::Metadata(MetadataScope::Graph(graph)) => format!(
            "READ_METADATA ON GRAPH {}",
            names
                .graph_name(*graph)
                .unwrap_or_else(|| format!("#{graph}"))
        ),
        Privilege::Metadata(MetadataScope::ControlPlane) => "READ_METADATA CONTROL PLANE".into(),
        Privilege::ExecutePreparedQuery { name } => format!("EXECUTE ON PREPARED QUERY {name}"),
    }
}

/// Attribute the covering source of one demanded row through the same probes
/// enforcement evaluates (`own ∪ PUBLIC`, expiry-aware). Tenancy is resolved by the
/// caller, mirroring `authorize_requirements`' per-graph tenancy shortcut.
fn attribute_row(
    subject: &Principal,
    privilege: &Privilege,
    sources: &dyn GrantSourceProbes,
) -> Option<CoverageSource> {
    if sources.holds_own_row(subject, privilege) {
        Some(CoverageSource::OwnGrant)
    } else if sources.holds_public_row(privilege) {
        Some(CoverageSource::PublicGrant)
    } else {
        None
    }
}

/// Build the coverage join for one requirement set in report mode.
///
/// This walk renders what [`authorize_requirements`] judges: identical structure
/// (per-graph tenancy shortcut, conjunctive all-of, any-one-group alternation,
/// tenancy-only unattributed residue, root-unattributed arm), no evaluation of its own.
pub(crate) fn build_coverage_report(
    reqs: &RequirementSet,
    root_graph_raw: u32,
    subject: &Principal,
    tenancy: &dyn GraphTenancy,
    sources: &dyn GrantSourceProbes,
    names: &dyn RequirementNames,
) -> CoverageReport {
    let mut covered = true;
    let root_unattributed = reqs.root_unattributed;
    if root_unattributed && !tenancy.is_tenant(root_graph_raw, subject) {
        covered = false;
    }
    let mut graphs = Vec::new();
    for (&graph_raw, demands) in &reqs.graphs {
        // Tenancy covers every demand on the graph at once (the enforcement shortcut).
        let tenant = tenancy.is_tenant(graph_raw, subject);
        let attribute = |privilege: &Privilege| -> Option<CoverageSource> {
            if tenant {
                Some(CoverageSource::Tenancy)
            } else {
                attribute_row(subject, privilege, sources)
            }
        };
        let mut conjunctive = Vec::with_capacity(demands.conjunctive.len());
        for privilege in &demands.conjunctive {
            let source = attribute(privilege);
            if source.is_none() {
                covered = false;
            }
            conjunctive.push(CoverageRow {
                requirement: describe_privilege(privilege, names),
                source,
            });
        }
        let mut alternatives = Vec::with_capacity(demands.alternatives.len());
        // Alternation semantics of `authorize_requirements`: the demand fails only when
        // NO group is fully covered; individual uncovered arms render informationally.
        let mut any_group_covered = demands.alternatives.is_empty();
        for group in &demands.alternatives {
            let mut arms = Vec::with_capacity(group.len());
            let mut group_covered = true;
            for privilege in group {
                let source = attribute(privilege);
                if source.is_none() {
                    group_covered = false;
                }
                arms.push(CoverageRow {
                    requirement: describe_privilege(privilege, names),
                    source,
                });
            }
            if group_covered {
                any_group_covered = true;
            }
            alternatives.push(arms);
        }
        if !any_group_covered {
            covered = false;
        }
        if demands.unattributed && !tenant {
            covered = false;
        }
        graphs.push(GraphCoverage {
            graph_raw,
            graph_name: names
                .graph_name(graph_raw)
                .unwrap_or_else(|| format!("#{graph_raw}")),
            conjunctive,
            alternatives,
            unattributed: demands.unattributed,
        });
    }
    CoverageReport {
        covered,
        root_graph_raw,
        graphs,
        root_unattributed,
    }
}

/// Rendering mode controlling source redaction ([ADR 0084] §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExplainRenderMode {
    /// Source classes only; never names another principal.
    SelfMode,
    /// Full row identities; owners already see every row via introspection.
    OwnerMode,
}

/// Source text of one coverage verdict under `mode`.
fn render_source(
    source: Option<CoverageSource>,
    mode: ExplainRenderMode,
    subject_text: &str,
) -> String {
    match source {
        Some(CoverageSource::Tenancy) => "graph tenancy".to_string(),
        Some(CoverageSource::PublicGrant) => "PUBLIC grant".to_string(),
        Some(CoverageSource::OwnGrant) => match mode {
            ExplainRenderMode::SelfMode => "your grant".to_string(),
            ExplainRenderMode::OwnerMode => format!("grant to {subject_text}"),
        },
        None => "UNCOVERED".to_string(),
    }
}

/// Render a [`CoverageReport`] as human-readable lines ([ADR 0084] §4): per-graph
/// conjunctive rows with sources, any-of alternation groups, unattributed residue as
/// "requires graph tenancy", and the root-unattributed arm. Conditional-policy
/// predicates do not appear: policies filter outputs, they never deny coverage.
pub(crate) fn render_coverage_report(
    report: &CoverageReport,
    mode: ExplainRenderMode,
    subject_text: &str,
) -> Vec<String> {
    let verdict = if report.covered {
        "COVERED"
    } else {
        "NOT COVERED"
    };
    let mut lines = vec![verdict.to_string()];
    for graph in &report.graphs {
        lines.push(format!("graph {}:", graph.graph_name));
        for row in &graph.conjunctive {
            lines.push(format!(
                "  {} — {}",
                row.requirement,
                render_source(row.source, mode, subject_text)
            ));
        }
        for arms in &graph.alternatives {
            lines.push("  ANY OF:".to_string());
            for arm in arms {
                lines.push(format!(
                    "    {} — {}",
                    arm.requirement,
                    render_source(arm.source, mode, subject_text)
                ));
            }
        }
        if graph.unattributed {
            lines.push("  UNATTRIBUTED READ — requires graph tenancy (owner/admin)".to_string());
        }
    }
    if report.root_unattributed {
        lines.push(
            "unattributed root-graph demand — requires graph tenancy (owner/admin)".to_string(),
        );
    }
    lines
}

/// Asker authority gate of EXPLAIN AUTHORIZATION ([ADR 0084] §3).
///
/// Self mode requires settled visibility of every touched graph under the admission
/// arms (`tenancy ∪ grant-derived ∪ elevation`); owner mode requires registry tenancy
/// of every touched graph. Any failure is the indistinguishable [`RouterError::NotFound`]
/// used everywhere else, so the diagnostic cannot become an existence oracle. The gate
/// takes no capability input by construction: caps confer no explain authority
/// ([ADR 0074] invariant 1).
pub(crate) fn authorize_explain_visibility(
    touched_graphs: impl IntoIterator<Item = u32>,
    asker: &Principal,
    owner_mode: bool,
    may_access: &dyn Fn(u32, &Principal) -> bool,
    is_tenant: &dyn Fn(u32, &Principal) -> bool,
) -> Result<(), RouterError> {
    for graph_raw in touched_graphs {
        let authorized = if owner_mode {
            is_tenant(graph_raw, asker)
        } else {
            may_access(graph_raw, asker)
        };
        if !authorized {
            return Err(RouterError::NotFound("prepared query".to_owned()));
        }
    }
    Ok(())
}

/// EXPLAIN AUTHORIZATION FOR PREPARED QUERY ([ADR 0084]): resolve the record exactly as
/// execution does, decide through enforcement's stored-primary/live-fallback duality,
/// gate the asker's authority over every touched graph, then render the coverage join.
///
/// Failure contract: unknown record and invisible touched graph yield indistinguishable
/// `NotFound`s. Nothing dispatches and nothing mutates stable state; the live fallback
/// replans onto the heap cache only, and a replan failure fails closed onto the stored
/// set instead of surfacing planner internals to the diagnostic caller.
pub(crate) fn explain_prepared_authorization(
    store: &RouterStore,
    asker: &Principal,
    query_name: &str,
    subject: &Principal,
    owner_mode: bool,
) -> Result<Vec<String>, RouterError> {
    let uniform_missing = || RouterError::NotFound(format!("prepared query {query_name:?}"));
    let key = crate::facade::stable::prepared_catalog::PreparedPlanKey::new(query_name);
    let v1 = crate::facade::stable::prepared_catalog::get_prepared_plan(&key)
        .ok_or_else(uniform_missing)?
        .as_v1()
        .clone();

    // Enforcement's exact duality: the stored set decides unless it denies, in which
    // case the live walk of the replanned current source decides (catalog drift rule).
    let stored_covers =
        requirements_cover(&v1.required_privileges, v1.graph_id.raw(), subject, store);
    let (reqs, covered) = if stored_covers {
        (v1.required_privileges.clone(), true)
    } else if let Some(plan) = crate::prepared::live_requirements_for_explanation(&key, &v1, *asker)
    {
        let live = extract_live(store, &plan, v1.graph_id);
        let live_covers = requirements_cover(&live, v1.graph_id.raw(), subject, store);
        (live, live_covers)
    } else {
        (v1.required_privileges.clone(), false)
    };

    // Authority gate before anything renders. Touched graphs conservatively include the
    // bound graph plus every graph named by either considered requirement set, so the
    // fallback can never disclose a graph the stored set hid.
    let mut touched: BTreeSet<u32> = reqs.graphs.keys().copied().collect();
    touched.extend(v1.required_privileges.graphs.keys().copied());
    touched.insert(v1.graph_id.raw());
    authorize_explain_visibility(
        touched,
        asker,
        owner_mode,
        &|graph_raw, asker| store.caller_may_access_graph(GraphId::from_raw(graph_raw), *asker),
        &|graph_raw, asker| store.is_graph_tenant(GraphId::from_raw(graph_raw), *asker),
    )
    .map_err(|_| uniform_missing())?;

    // Render the join with the production probes (same rows, same expiry semantics).
    let names = StoreRequirementNames { store };
    let report = build_coverage_report(
        &reqs,
        v1.graph_id.raw(),
        subject,
        &StoreTenancy { store },
        &StoredGrantSources,
        &names,
    );
    debug_assert_eq!(report.covered, covered, "report walk must mirror verdict");
    let mode = if owner_mode {
        ExplainRenderMode::OwnerMode
    } else {
        ExplainRenderMode::SelfMode
    };
    Ok(render_coverage_report(&report, mode, &subject.to_text()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_auth::GrantState;
    use gleaph_gql_planner::plan::Str;
    use ic_stable_structures::DefaultMemoryImpl;
    use std::collections::BTreeSet;

    const NOW_NS: u64 = 1_000;
    const GRAPH_RAW: u32 = 7;

    fn graph() -> GraphId {
        GraphId::from_raw(GRAPH_RAW)
    }

    fn node_label(name: &str) -> Option<NodeLabelRef> {
        Some(NodeLabelRef::new(name))
    }

    /// Fixed vocabulary: Person=1, Employee=2; age=20, name=21, secret=22;
    /// KNOWS=10 directed, LIKES=11 undirected, MENTIONS=12 undeclared.
    fn catalog() -> FakeCatalog {
        FakeCatalog {
            vertices: vec![("Person", 1), ("Employee", 2)],
            edges: vec![
                ("KNOWS", 10, Some(false)),
                ("LIKES", 11, Some(true)),
                ("MENTIONS", 12, None),
            ],
            properties: vec![("age", 20), ("name", 21), ("secret", 22)],
            vector_indexes: Vec::new(),
            property_index_labels: Vec::new(),
        }
    }

    struct FakeCatalog {
        vertices: Vec<(&'static str, u32)>,
        edges: Vec<(&'static str, u32, Option<bool>)>,
        properties: Vec<(&'static str, u32)>,
        vector_indexes: Vec<(&'static str, VectorIndexSource)>,
        /// Active vertex property index bindings: property id → the one label the index
        /// is bound to. Empty by default; multiple entries for one id model an ambiguous
        /// (conflicting) index catalog.
        property_index_labels: Vec<(u32, &'static str)>,
    }

    impl PlanCatalogView for FakeCatalog {
        fn graph_by_name(&self, name: &str) -> Option<GraphId> {
            match name {
                "other" => Some(GraphId::from_raw(9)),
                _ => None,
            }
        }

        fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32> {
            if graph.raw() != GRAPH_RAW {
                return None;
            }
            self.vertices
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, id)| *id)
        }

        fn edge_label(&self, graph: GraphId, name: &str) -> Option<(u32, Option<bool>)> {
            if graph.raw() != GRAPH_RAW {
                return None;
            }
            self.edges
                .iter()
                .find(|(candidate, _, _)| *candidate == name)
                .map(|(_, id, undirected)| (*id, *undirected))
        }

        fn property(&self, graph: GraphId, name: &str) -> Option<u32> {
            if graph.raw() != GRAPH_RAW {
                return None;
            }
            self.properties
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, id)| *id)
        }

        fn vector_index_source(
            &self,
            _graph: GraphId,
            index_name: &str,
        ) -> Option<VectorIndexSource> {
            self.vector_indexes
                .iter()
                .find(|(candidate, _)| *candidate == index_name)
                .map(|(_, source)| source.clone())
        }

        fn vertex_property_index_label(&self, graph: GraphId, property_id: u32) -> Option<String> {
            if graph.raw() != GRAPH_RAW {
                return None;
            }
            let matches: Vec<&&'static str> = self
                .property_index_labels
                .iter()
                .filter(|(candidate_property, _)| *candidate_property == property_id)
                .map(|(_, label)| label)
                .collect();
            match matches.as_slice() {
                [label] => Some((*label).to_string()),
                _ => None,
            }
        }
    }

    /// Real [`GrantState`] storage semantics behind the evaluation trait, mirroring
    /// `StoredGrants` (caller ∪ PUBLIC, expiry-aware) at a fixed `now`.
    struct GrantTable {
        state: GrantState<DefaultMemoryImpl>,
    }

    impl Default for GrantTable {
        fn default() -> Self {
            Self {
                state: GrantState::init(DefaultMemoryImpl::default()),
            }
        }
    }

    impl GrantTable {
        fn grant(&mut self, subject: GrantSubject, privilege: &Privilege) {
            self.state
                .grant(subject, privilege, None, None)
                .expect("grant");
        }

        fn grant_expiring(&mut self, subject: GrantSubject, privilege: &Privilege, at: u64) {
            self.state
                .grant(subject, privilege, Some(at), None)
                .expect("grant");
        }

        fn traverse(&mut self, subject: GrantSubject, direction: Option<Direction>, label: u32) {
            self.grant(
                subject,
                &Privilege::Graph(GraphPrivilege {
                    graph: GRAPH_RAW,
                    operation: GraphOperation::Traverse(direction),
                    resource: GraphResource::EdgeLabel(label),
                }),
            );
        }

        fn vertex(&mut self, subject: GrantSubject, operation: GraphOperation, label: u32) {
            self.grant(
                subject,
                &Privilege::Graph(GraphPrivilege {
                    graph: GRAPH_RAW,
                    operation,
                    resource: GraphResource::VertexLabel(label),
                }),
            );
        }

        fn vertex_property(&mut self, subject: GrantSubject, label: u32, property: u32) {
            self.grant(
                subject,
                &Privilege::Graph(GraphPrivilege {
                    graph: GRAPH_RAW,
                    operation: GraphOperation::ReadProperty,
                    resource: GraphResource::VertexProperty { label, property },
                }),
            );
        }
    }

    impl EffectiveGrants for GrantTable {
        fn covers(&self, caller: &Principal, privilege: &Privilege) -> bool {
            self.state
                .holds(GrantSubject::effective_for(caller), privilege, NOW_NS)
                || self.state.holds(GrantSubject::Public, privilege, NOW_NS)
        }

        fn holds_any_vertex_label_match(&self, caller: &Principal, graph: u32) -> bool {
            self.state.holds_any_vertex_label_match(
                GrantSubject::effective_for(caller),
                graph,
                NOW_NS,
            ) || self
                .state
                .holds_any_vertex_label_match(GrantSubject::Public, graph, NOW_NS)
        }
    }

    struct TenantSet(BTreeSet<u32>);

    impl GraphTenancy for TenantSet {
        fn is_tenant(&self, graph_raw: u32, _caller: &Principal) -> bool {
            self.0.contains(&graph_raw)
        }
    }

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    fn allowed(
        reqs: &RequirementSet,
        caller: &Principal,
        grants: &dyn EffectiveGrants,
        tenants: &[u32],
    ) -> bool {
        authorize_requirements(
            reqs,
            GRAPH_RAW,
            caller,
            grants,
            &TenantSet(tenants.iter().copied().collect()),
        )
    }

    /// Deliberately wrong evaluator used as a probe: it must NOT satisfy the denial
    /// assertions that the real evaluation passes.
    struct AlwaysAllow;

    impl EffectiveGrants for AlwaysAllow {
        fn covers(&self, _: &Principal, _: &Privilege) -> bool {
            true
        }

        fn holds_any_vertex_label_match(&self, _: &Principal, _: u32) -> bool {
            true
        }
    }

    fn extract_ops(ops: Vec<PlanOp>) -> RequirementSet {
        extract(&PhysicalPlan::from_ops(ops), graph(), &catalog())
    }

    fn projection(keys: &[&str]) -> Option<Rc<[Str]>> {
        Some(keys.iter().map(|k| Rc::from(*k)).collect())
    }

    /// Scan with an explicit (possibly empty) hydration list.
    fn scan(var: &str, label: Option<&str>, projection_keys: &[&str]) -> PlanOp {
        PlanOp::NodeScan {
            variable: var.into(),
            label: label.map(NodeLabelRef::new),
            property_projection: Some(
                projection_keys
                    .iter()
                    .map(|k| Rc::from(*k))
                    .collect::<Rc<[Str]>>(),
            ),
        }
    }

    /// Scan retaining the full property map (`property_projection: None`).
    fn full_map_scan(var: &str, label: Option<&str>) -> PlanOp {
        PlanOp::NodeScan {
            variable: var.into(),
            label: label.map(NodeLabelRef::new),
            property_projection: None,
        }
    }

    /// Expand hop with only the fields under test populated.
    fn expand(direction: EdgeDirection, label: Option<&str>) -> PlanOp {
        expand_with(direction, label, None)
    }

    fn expand_with(
        direction: EdgeDirection,
        label: Option<&str>,
        label_expr: Option<LabelExpr>,
    ) -> PlanOp {
        PlanOp::Expand {
            src: "a".into(),
            edge: "__e".into(),
            dst: "b".into(),
            direction,
            label: label.map(EdgeLabelRef::new),
            label_expr,
            var_len: None,
            indexed_edge_equality: None,
            edge_inline_property_predicate: None,
            edge_inline_vector_predicate: None,
            edge_property_projection: projection(&[]),
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        }
    }

    fn traverse_row(direction: Option<Direction>, label: u32) -> Privilege {
        Privilege::Graph(GraphPrivilege {
            graph: GRAPH_RAW,
            operation: GraphOperation::Traverse(direction),
            resource: GraphResource::EdgeLabel(label),
        })
    }

    /// The traversal rows a plan demands, as a sorted multiset for exact assertions.
    fn demanded_traversal_rows(ops: Vec<PlanOp>) -> Vec<Privilege> {
        let reqs = extract_ops(ops);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("graph demands present");
        assert!(!demands.unattributed, "traversal must be attributable");
        let mut rows = demands.conjunctive.clone();
        rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        rows
    }

    #[test]
    fn direction_matrix_over_directed_label() {
        // Declared DIRECTED label: one directional row per orientation; every
        // undirected pattern shape requires BOTH rows (ADR 0074 §2).
        let expected: [(EdgeDirection, Vec<Privilege>); 7] = [
            (
                EdgeDirection::PointingRight,
                vec![traverse_row(Some(Direction::Outgoing), 10)],
            ),
            (
                EdgeDirection::PointingLeft,
                vec![traverse_row(Some(Direction::Incoming), 10)],
            ),
            (
                EdgeDirection::LeftOrRight,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::Undirected,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::LeftOrUndirected,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::UndirectedOrRight,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::AnyDirection,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
        ];
        for (direction, want) in expected {
            assert_eq!(
                demanded_traversal_rows(vec![expand(direction, Some("KNOWS"))]),
                want,
                "direction {direction:?}"
            );
        }
    }

    #[test]
    fn direction_matrix_over_undirected_label() {
        // Declared UNDIRECTED label: only the unoriented row exists, for every shape.
        for direction in [
            EdgeDirection::PointingRight,
            EdgeDirection::PointingLeft,
            EdgeDirection::LeftOrRight,
            EdgeDirection::Undirected,
            EdgeDirection::LeftOrUndirected,
            EdgeDirection::UndirectedOrRight,
            EdgeDirection::AnyDirection,
        ] {
            assert_eq!(
                demanded_traversal_rows(vec![expand(direction, Some("LIKES"))]),
                vec![traverse_row(None, 11)],
                "direction {direction:?}"
            );
        }
    }

    #[test]
    fn undeclared_directedness_takes_unoriented_row() {
        // Grant-side fail-closure means only the unoriented row can exist for labels
        // whose directedness the schema does not declare.
        assert_eq!(
            demanded_traversal_rows(vec![expand(EdgeDirection::PointingRight, Some("MENTIONS"))]),
            vec![traverse_row(None, 12)]
        );
    }

    #[test]
    fn reverse_traversal_denied_like_any_other_demand() {
        let forward = extract_ops(vec![
            scan("a", Some("Person"), &[]),
            expand(EdgeDirection::PointingRight, Some("KNOWS")),
        ]);
        let reverse = extract_ops(vec![
            scan("a", Some("Person"), &[]),
            expand(EdgeDirection::PointingLeft, Some("KNOWS")),
        ]);
        let mut grants = GrantTable::default();
        let caller = principal(1);
        grants.vertex(GrantSubject::Principal(caller), GraphOperation::Match, 1);
        grants.traverse(
            GrantSubject::Principal(caller),
            Some(Direction::Outgoing),
            10,
        );

        // Forward pattern admitted...
        assert!(allowed(&forward, &caller, &grants, &[]));
        // ...reverse pattern uniformly denied: same boolean outcome that maps to the
        // same Forbidden error as any other uncovered demand.
        assert!(!allowed(&reverse, &caller, &grants, &[]));
        // Wrong-implementation probe: an evaluator granting everything cannot satisfy
        // these assertions — it would admit the reverse pattern too.
        assert!(allowed(&reverse, &caller, &AlwaysAllow, &[]));
    }

    /// ADR 0089 §4 follow-up: an unconstrained scan whose downstream output reads no
    /// physical properties is a topology-only scan even when the planner leaves
    /// `property_projection: None` (no output at all). It takes the marker path.
    #[test]
    fn unlabeled_scan_with_no_output_emits_marker() {
        let reqs = extract_ops(vec![full_map_scan("n", None)]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(
            demands.unconstrained_vertex_match,
            "unconstrained scan with no output must set the marker (topology-only)"
        );
        assert!(
            !demands.unattributed,
            "unconstrained scan with no output must NOT be tenancy-only"
        );
    }

    /// ADR 0089 §2: an unconstrained scan with an empty property projection emits the
    /// marker demand ("caller must hold `MATCH` on at least one vertex label") instead
    /// of tenancy-only. The per-bucket restriction is applied at request build time.
    #[test]
    fn unconstrained_empty_projection_scan_emits_marker() {
        let reqs = extract_ops(vec![scan("n", None, &[])]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(
            demands.unconstrained_vertex_match,
            "empty-projection unconstrained scan must set the marker"
        );
        assert!(
            !demands.unattributed,
            "empty-projection unconstrained scan must NOT be tenancy-only"
        );
        assert!(demands.conjunctive.is_empty());

        // A caller with no vertex-label MATCH is rejected by the marker.
        let grants = GrantTable::default();
        assert!(!allowed(&reqs, &principal(1), &grants, &[]));

        // A caller with a concrete vertex-label MATCH row passes the marker.
        let mut grants = GrantTable::default();
        grants.vertex(
            GrantSubject::Principal(principal(1)),
            GraphOperation::Match,
            1,
        );
        assert!(allowed(&reqs, &principal(1), &grants, &[]));
    }

    /// ADR 0089 §4: an explicit non-empty `property_projection` keeps the scan
    /// tenancy-only (fail closed) because the planner is asking the executor to
    /// hydrate those properties. Even if the downstream output doesn't reference
    /// them, the values are atomic.
    #[test]
    fn unconstrained_explicit_property_projection_stays_tenancy_only() {
        let reqs = extract_ops(vec![scan("n", None, &["name"])]);
        assert!(
            reqs.graphs.get(&GRAPH_RAW).unwrap().unattributed,
            "explicit non-empty property_projection must stay tenancy-only"
        );
    }

    /// ADR 0089 §4: an unconstrained scan whose downstream output references a
    /// vertex property (`RETURN a.name`) stays tenancy-only because the property
    /// value is atomic and must be covered by a `READ_PROPERTY` row.
    #[test]
    fn unconstrained_output_with_property_access_stays_tenancy_only() {
        use gleaph_gql::ast::Expr;
        let plan = PhysicalPlan::from_ops(vec![
            full_map_scan("n", None),
            PlanOp::Project {
                columns: vec![gleaph_gql_planner::plan::ProjectColumn {
                    expr: Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::var("n")),
                        property: "name".into(),
                    }),
                    alias: None,
                }],
                distinct: false,
            },
        ]);
        let reqs = extract(&plan, graph(), &catalog());
        assert!(
            reqs.graphs.get(&GRAPH_RAW).unwrap().unattributed,
            "RETURN a.name on an unconstrained scan must stay tenancy-only"
        );
    }

    /// ADR 0089 §4: an unconstrained scan whose downstream output is `ElementId(a)`
    /// reads no physical property — it is topology-only and takes the marker path.
    #[test]
    fn unconstrained_output_with_element_id_takes_marker() {
        use gleaph_gql::ast::Expr;
        let plan = PhysicalPlan::from_ops(vec![
            full_map_scan("a", None),
            PlanOp::Project {
                columns: vec![gleaph_gql_planner::plan::ProjectColumn {
                    expr: Expr::new(ExprKind::ElementId(Box::new(Expr::var("a")))),
                    alias: None,
                }],
                distinct: false,
            },
        ]);
        let reqs = extract(&plan, graph(), &catalog());
        assert!(
            reqs.graphs
                .get(&GRAPH_RAW)
                .unwrap()
                .unconstrained_vertex_match,
            "RETURN element_id(a) on an unconstrained scan must take the marker path"
        );
        assert!(
            !reqs.graphs.get(&GRAPH_RAW).unwrap().unattributed,
            "RETURN element_id(a) must NOT be tenancy-only"
        );
    }

    /// ADR 0089 §4: `RETURN count(a)` on an unconstrained scan reads no property —
    /// it is topology-only and takes the marker path.
    #[test]
    fn unconstrained_output_with_count_takes_marker() {
        use gleaph_gql::ast::Expr;
        let plan = PhysicalPlan::from_ops(vec![
            full_map_scan("a", None),
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![gleaph_gql_planner::plan::AggregateSpec {
                    func: gleaph_gql::ast::AggregateFunc::Count,
                    expr: Some(Expr::var("a")),
                    expr2: None,
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: None,
                }],
            },
        ]);
        let reqs = extract(&plan, graph(), &catalog());
        assert!(
            reqs.graphs
                .get(&GRAPH_RAW)
                .unwrap()
                .unconstrained_vertex_match,
            "RETURN count(a) on an unconstrained scan must take the marker path"
        );
        assert!(
            !reqs.graphs.get(&GRAPH_RAW).unwrap().unattributed,
            "RETURN count(a) must NOT be tenancy-only"
        );
    }

    /// ADR 0089 §5: the wildcard `NODES *` row covers any vertex-label MATCH check.
    #[test]
    fn wildcard_vertex_label_grant_covers_marker() {
        let reqs = extract_ops(vec![scan("n", None, &[])]);
        let mut grants = GrantTable::default();
        // Grant the wildcard AllVertexLabels row instead of any concrete label.
        grants.grant(
            GrantSubject::Principal(principal(1)),
            &Privilege::Graph(GraphPrivilege {
                graph: GRAPH_RAW,
                operation: GraphOperation::Match,
                resource: GraphResource::AllVertexLabels,
            }),
        );
        assert!(allowed(&reqs, &principal(1), &grants, &[]));
    }

    fn grants_state_with_all_listed_rows() -> GrantTable {
        let mut grants = GrantTable::default();
        grants.vertex(GrantSubject::Public, GraphOperation::Match, 1);
        grants.vertex(GrantSubject::Public, GraphOperation::Read, 1);
        grants.vertex_property(GrantSubject::Public, 1, 21);
        grants.traverse(GrantSubject::Public, Some(Direction::Outgoing), 10);
        grants
    }

    #[test]
    fn labeled_scan_projection_contract() {
        let full_map = extract_ops(vec![full_map_scan("n", Some("Person"))]);
        let empty_projection = extract_ops(vec![scan("n", Some("Person"), &[])]);
        let explicit_name = extract_ops(vec![scan("n", Some("Person"), &["name"])]);

        let mut grants = GrantTable::default();
        let reader = principal(9);
        grants.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);

        // MATCH alone admits only the no-hydration projection.
        assert!(allowed(&empty_projection, &reader, &grants, &[]));
        assert!(!allowed(&full_map, &reader, &grants, &[]));
        assert!(!allowed(&explicit_name, &reader, &grants, &[]));

        // READ covers the whole-map demand but NOT an enumerated property demand.
        grants.vertex(GrantSubject::Principal(reader), GraphOperation::Read, 1);
        assert!(allowed(&full_map, &reader, &grants, &[]));
        assert!(!allowed(&explicit_name, &reader, &grants, &[]));

        // The enumerated READ_PROPERTY row admits exactly its own key.
        grants.vertex_property(GrantSubject::Principal(reader), 1, 21);
        assert!(allowed(&explicit_name, &reader, &grants, &[]));
        // ...and a different projected key stays denied.
        let explicit_secret = extract_ops(vec![scan("n", Some("Person"), &["secret"])]);
        assert!(!allowed(&explicit_secret, &reader, &grants, &[]));
    }

    #[test]
    fn filter_and_project_reads_are_attributed_through_label_facts() {
        let reqs = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::PropertyFilter {
                predicates: vec![property_access("n", "age")],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![gleaph_gql_planner::plan::ProjectColumn {
                    expr: property_access("n", "name"),
                    alias: None,
                }],
                distinct: false,
            },
        ]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        let rendered: BTreeSet<String> = demands
            .conjunctive
            .iter()
            .map(|p| format!("{p:?}"))
            .collect();
        assert!(rendered.contains(&format!(
            "{:?}",
            vertex_row(GRAPH_RAW, GraphOperation::Match, 1)
        )));
        assert!(rendered.contains(&format!("{:?}", vertex_property_row(GRAPH_RAW, 1, 20))));
        assert!(rendered.contains(&format!("{:?}", vertex_property_row(GRAPH_RAW, 1, 21))));

        // A read through a variable bound to two different labels degrades to
        // tenancy-only instead of guessing one label.
        let ambiguous = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            scan("n", Some("Employee"), &[]),
            PlanOp::PropertyFilter {
                predicates: vec![property_access("n", "age")],
                stage: 0,
            },
        ]);
        assert!(ambiguous.graphs.get(&GRAPH_RAW).unwrap().unattributed);
    }

    fn property_access(var: &str, property: &str) -> Expr {
        use gleaph_gql::ast::ExprKind;
        Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable(var.into()))),
                property: property.into(),
            })),
            op: gleaph_gql::ast::CmpOp::Gt,
            right: Box::new(Expr::new(ExprKind::Variable("x".into()))),
        })
    }

    // ───── Index-anchored vertex scans: label resolution from the index catalog ─────

    /// Catalog whose `age` (20) carries one active vertex index bound to `label`.
    fn indexed_catalog(label: &'static str) -> FakeCatalog {
        let mut catalog = catalog();
        catalog.property_index_labels = vec![(20, label)];
        catalog
    }

    fn index_scan(var: &str, property: &str, projection_keys: Option<&[&str]>) -> PlanOp {
        PlanOp::IndexScan {
            variable: var.into(),
            property: property.into(),
            value: gleaph_gql_planner::plan::ScanValue::Parameter("p".into()),
            cmp: gleaph_gql::ast::CmpOp::Eq,
            property_projection: projection_keys
                .map(|keys| keys.iter().map(|k| Rc::from(*k)).collect::<Rc<[Str]>>()),
            ordered_by_sort: None,
        }
    }

    fn extract_with(ops: Vec<PlanOp>, catalog: &FakeCatalog) -> RequirementSet {
        extract(&PhysicalPlan::from_ops(ops), graph(), catalog)
    }

    #[test]
    fn index_anchored_scan_attributes_like_a_labeled_node_scan() {
        // age=20 is uniquely bound to a Person index: the anchor demands the NodeScan
        // contract rows (Match + full-map Read) and its deferred probe read attributes
        // through the index-bound label. The residual equality filter re-defers the same
        // read; identical pending reads collapse to one demand row.
        let reqs = extract_with(
            vec![
                index_scan("n", "age", None),
                PlanOp::PropertyFilter {
                    predicates: vec![property_access("n", "age")],
                    stage: 0,
                },
            ],
            &indexed_catalog("Person"),
        );
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
                vertex_row(GRAPH_RAW, GraphOperation::Read, 1),
                vertex_property_row(GRAPH_RAW, 1, 20),
            ]
        );

        // A non-owner holding exactly those rows executes; missing any one denies.
        let mut grants = GrantTable::default();
        grants.vertex(GrantSubject::Public, GraphOperation::Match, 1);
        grants.vertex(GrantSubject::Public, GraphOperation::Read, 1);
        grants.vertex_property(GrantSubject::Public, 1, 20);
        assert!(allowed(&reqs, &principal(1), &grants, &[]));
        let mut partial = GrantTable::default();
        partial.vertex(GrantSubject::Public, GraphOperation::Match, 1);
        partial.vertex_property(GrantSubject::Public, 1, 20);
        assert!(!allowed(&reqs, &principal(1), &partial, &[]));
    }

    #[test]
    fn index_anchored_scan_projection_demands_per_key_rows() {
        // An explicit hydration list keeps Match, drops the full-map Read, and demands
        // one ReadProperty per projected key. The arm's own projection deferral resolves
        // again at scope end, so the projected key extracts twice — the same benign
        // duplicate a labeled NodeScan projection plus a downstream read of the same key
        // produces; coverage is per-row, so the duplicate changes no admission outcome.
        let reqs = extract_with(
            vec![index_scan("n", "age", Some(&["name"]))],
            &indexed_catalog("Person"),
        );
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
                vertex_property_row(GRAPH_RAW, 1, 21),
                vertex_property_row(GRAPH_RAW, 1, 20),
                vertex_property_row(GRAPH_RAW, 1, 21),
            ]
        );
    }

    #[test]
    fn unresolvable_index_label_keeps_the_anchor_tenancy_only() {
        // No index on the probe property: no label fact, no scan rows, deferred probe
        // read fails closed — today's pre-fix behavior is preserved fail-closed.
        let reqs = extract_ops(vec![index_scan("n", "age", None)]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(demands.unattributed);
        assert!(demands.conjunctive.is_empty());
    }

    #[test]
    fn conflicting_index_labels_fail_closed() {
        // Two active indexes bind the same property to DIFFERENT labels — the catalog
        // cannot prove which label the scan binds, so no label fact is noted.
        let mut catalog = catalog();
        catalog.property_index_labels = vec![(20, "Person"), (20, "Employee")];
        let reqs = extract_with(vec![index_scan("n", "age", None)], &catalog);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(demands.unattributed);
        assert!(demands.conjunctive.is_empty());
    }

    #[test]
    fn index_intersection_demands_one_consistent_label() {
        // Every intersected scan resolves to the same uniquely-indexed label: the shared
        // variable's reads attribute like a labeled scan.
        let mut catalog = indexed_catalog("Person");
        catalog.property_index_labels.push((21, "Person"));
        let reqs = extract_with(
            vec![PlanOp::IndexIntersection {
                variable: "n".into(),
                scans: vec![intersect_spec("age"), intersect_spec("name")],
                property_projection: projection(&["age"]),
            }],
            &catalog,
        );
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_property_row(GRAPH_RAW, 1, 20),
                vertex_property_row(GRAPH_RAW, 1, 21),
            ]
        );
    }

    #[test]
    fn index_intersection_with_conflicting_labels_fails_closed() {
        let mut catalog = catalog();
        catalog.property_index_labels = vec![(20, "Person"), (21, "Employee")];
        let reqs = extract_with(
            vec![PlanOp::IndexIntersection {
                variable: "n".into(),
                scans: vec![intersect_spec("age"), intersect_spec("name")],
                property_projection: None,
            }],
            &catalog,
        );
        assert!(reqs.graphs.get(&GRAPH_RAW).expect("demands").unattributed);
    }

    fn intersect_spec(property: &str) -> gleaph_gql_planner::plan::IndexScanSpec {
        gleaph_gql_planner::plan::IndexScanSpec {
            property: property.into(),
            value: gleaph_gql_planner::plan::ScanValue::Parameter("p".into()),
            cmp: gleaph_gql::ast::CmpOp::Eq,
        }
    }

    #[test]
    fn conditional_index_scan_notes_one_agreed_label() {
        // All candidate properties resolve to the one label the fallback scan also binds:
        // deferred reads attribute through it.
        let mut catalog = indexed_catalog("Person");
        catalog.property_index_labels.push((21, "Person"));
        let reqs = extract_with(
            vec![PlanOp::ConditionalIndexScan {
                candidates: vec![conditional_candidate("age"), conditional_candidate("name")],
                fallback_label: Some(NodeLabelRef::new("Person")),
                fallback_variable: "n".into(),
                property_projection: None,
            }],
            &catalog,
        );
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_property_row(GRAPH_RAW, 1, 20),
                vertex_property_row(GRAPH_RAW, 1, 21),
            ]
        );
    }

    #[test]
    fn conditional_index_scan_without_fallback_notes_resolved_label() {
        let reqs = extract_with(
            vec![PlanOp::ConditionalIndexScan {
                candidates: vec![conditional_candidate("age")],
                fallback_label: None,
                fallback_variable: "n".into(),
                property_projection: None,
            }],
            &indexed_catalog("Person"),
        );
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        assert_eq!(
            demands.conjunctive,
            vec![vertex_property_row(GRAPH_RAW, 1, 20)]
        );
    }

    #[test]
    fn conditional_index_scan_disagreement_fails_closed() {
        // Candidate resolutions disagree with the fallback label: the variable may bind
        // vertices of either label, so no label fact is noted (fail closed).
        let catalog = indexed_catalog("Employee");
        let reqs = extract_with(
            vec![PlanOp::ConditionalIndexScan {
                candidates: vec![conditional_candidate("age")],
                fallback_label: Some(NodeLabelRef::new("Person")),
                fallback_variable: "n".into(),
                property_projection: None,
            }],
            &catalog,
        );
        assert!(reqs.graphs.get(&GRAPH_RAW).expect("demands").unattributed);

        // Two candidates resolving to DIFFERENT labels are equally ambiguous.
        let split = split_catalog();
        let multi = extract_with(
            vec![PlanOp::ConditionalIndexScan {
                candidates: vec![conditional_candidate("age"), conditional_candidate("name")],
                fallback_label: None,
                fallback_variable: "n".into(),
                property_projection: None,
            }],
            &split,
        );
        assert!(multi.graphs.get(&GRAPH_RAW).expect("demands").unattributed);
    }

    fn conditional_candidate(property: &str) -> gleaph_gql_planner::plan::ConditionalScanCandidate {
        gleaph_gql_planner::plan::ConditionalScanCandidate {
            param_name: "p".into(),
            property: property.into(),
            variable: "n".into(),
            cmp: gleaph_gql::ast::CmpOp::Eq,
        }
    }

    /// Fresh vocabulary catalog with one index binding per property (Person→age,
    /// Employee→name) modeling a split-index catalog.
    fn split_catalog() -> FakeCatalog {
        let mut catalog = catalog();
        catalog.property_index_labels = vec![(20, "Person"), (21, "Employee")];
        catalog
    }

    #[test]
    fn public_row_enables_anonymous_adhoc_reads() {
        let reqs = extract_ops(vec![
            scan("n", Some("Person"), &["name"]),
            expand(EdgeDirection::PointingRight, Some("KNOWS")),
        ]);
        let grants = grants_state_with_all_listed_rows();
        assert!(
            allowed(&reqs, &Principal::anonymous(), &grants, &[]),
            "anonymous evaluation resolves the PUBLIC baseline"
        );
        // Expiry awareness: the same rows expired before `now` cover nothing.
        let mut expired = GrantTable::default();
        expired.grant_expiring(
            GrantSubject::Public,
            &vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
            NOW_NS - 1,
        );
        expired.grant_expiring(
            GrantSubject::Public,
            &vertex_property_row(GRAPH_RAW, 1, 21),
            NOW_NS - 1,
        );
        expired.grant_expiring(
            GrantSubject::Public,
            &traverse_row(Some(Direction::Outgoing), 10),
            NOW_NS - 1,
        );
        let minimal = extract_ops(vec![scan("n", Some("Person"), &[])]);
        assert!(!allowed(&minimal, &Principal::anonymous(), &expired, &[]));
    }

    #[test]
    fn mutation_matrix_demands_operation_specific_rows() {
        let reader = principal(3);
        let insert_vertex = extract_ops(vec![PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec![NodeLabelRef::new("Person")],
            properties: vec![assignment("name", "n")],
        }]);
        let insert_edge = extract_ops(vec![PlanOp::InsertEdge {
            variable: Some("e".into()),
            src: "a".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            labels: vec![EdgeLabelRef::new("KNOWS")],
            properties: vec![assignment("since", "e")],
        }]);
        let set_property = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::SetProperties {
                items: vec![SetPlanItem::Property {
                    variable: "n".into(),
                    property: "age".into(),
                    value: literal_text("5"),
                }],
            },
        ]);
        let set_label = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::SetProperties {
                items: vec![SetPlanItem::Label {
                    variable: "n".into(),
                    label: NodeLabelRef::new("Employee"),
                }],
            },
        ]);
        let remove_property = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::RemoveProperties {
                items: vec![RemovePlanItem::Property {
                    variable: "n".into(),
                    property: "age".into(),
                }],
            },
        ]);
        let remove_label = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::RemoveProperties {
                items: vec![RemovePlanItem::Label {
                    variable: "n".into(),
                    label: NodeLabelRef::new("Employee"),
                }],
            },
        ]);
        let delete_vertex = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::DeleteVertex {
                variable: "n".into(),
            },
        ]);
        let detach_delete = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::DetachDeleteVertex {
                variable: "n".into(),
            },
        ]);
        let delete_edge = extract_ops(vec![
            scan("a", Some("Person"), &[]),
            PlanOp::Expand {
                src: "a".into(),
                edge: "k".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: node_label_of_edge("KNOWS"),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_inline_property_predicate: None,
                edge_inline_vector_predicate: None,
                edge_property_projection: projection(&[]),
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
                near_group_var: None,
                far_group_var: None,
                path_var: None,
                emit_path_binding: false,
            },
            PlanOp::DeleteEdge {
                variable: "k".into(),
            },
        ]);

        // Nothing is covered without rows.
        let empty = GrantTable::default();
        for (name, reqs) in [
            ("insert_vertex", &insert_vertex),
            ("insert_edge", &insert_edge),
            ("set_property", &set_property),
            ("set_label", &set_label),
            ("remove_property", &remove_property),
            ("remove_label", &remove_label),
            ("delete_vertex", &delete_vertex),
            ("detach_delete", &detach_delete),
            ("delete_edge", &delete_edge),
        ] {
            assert!(
                !allowed(reqs, &reader, &empty, &[]),
                "{name} must be denied without rows"
            );
        }

        // Detach-delete additionally destroys incident edges of unlisted labels, so even
        // full per-label rows stay tenancy-only.
        let mut full_rows = GrantTable::default();
        fully_granted(&mut full_rows);
        assert!(
            !allowed(&detach_delete, &reader, &full_rows, &[]),
            "DETACH DELETE is tenancy-only in Phase 1"
        );

        // Each operation is admitted by exactly its own row(s).
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Create, 1);
        assert!(allowed(&insert_vertex, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.grant(
            GrantSubject::Principal(reader),
            &edge_row(GRAPH_RAW, GraphOperation::Create, 10),
        );
        assert!(allowed(&insert_edge, &reader, &g, &[]));
        let mut g = GrantTable::default();
        // The anchor scan needs MATCH; the mutation itself needs UPDATE.
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Update, 1);
        assert!(allowed(&set_property, &reader, &g, &[]));
        assert!(allowed(&remove_property, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Update, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Update, 2);
        assert!(allowed(&set_label, &reader, &g, &[]));
        assert!(allowed(&remove_label, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Delete, 1);
        assert!(allowed(&delete_vertex, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.grant(
            GrantSubject::Principal(reader),
            &edge_row(GRAPH_RAW, GraphOperation::Delete, 10),
        );
        g.traverse(
            GrantSubject::Principal(reader),
            Some(Direction::Outgoing),
            10,
        );
        assert!(allowed(&delete_edge, &reader, &g, &[]));
    }

    fn node_label_of_edge(name: &str) -> Option<EdgeLabelRef> {
        Some(EdgeLabelRef::new(name))
    }

    fn fully_granted(grants: &mut GrantTable) -> &GrantTable {
        grants.vertex(
            GrantSubject::Principal(principal(3)),
            GraphOperation::Match,
            1,
        );
        grants.vertex(
            GrantSubject::Principal(principal(3)),
            GraphOperation::Delete,
            1,
        );
        grants
    }

    fn assignment(name: &str, _var: &str) -> gleaph_gql_planner::plan::PropertyAssignment {
        gleaph_gql_planner::plan::PropertyAssignment {
            name: name.into(),
            // Literal values keep insert demands exactly CREATE; no expression reads.
            value: literal_text(name),
        }
    }

    fn literal_text(text: &str) -> Expr {
        Expr::new(ExprKind::Literal(gleaph_gql::Value::Text(text.into())))
    }

    #[test]
    fn caps_are_not_an_input_to_evaluation() {
        // Structural guarantee (ADR 0074 invariant 1): the evaluation surface accepts a
        // caller identity and probe traits only. There is no capability parameter to
        // leak, so this test pins the denial of a caps-shaped-but-grantless caller.
        let reqs = extract_ops(vec![scan("n", Some("Person"), &[])]);
        assert!(!allowed(&reqs, &principal(4), &GrantTable::default(), &[]));
    }

    #[test]
    fn permissive_stub_evaluator_fails_the_denial_matrix() {
        // Wrong-implementation probe required by the plan: a stub evaluator that grants
        // everything must fail this matrix. Every denial assertion above would break if
        // the real evaluator behaved like `AlwaysAllow`; this asserts the discriminator
        // directly.
        let denied_without_rows =
            extract_ops(vec![expand(EdgeDirection::PointingLeft, Some("KNOWS"))]);
        let real = GrantTable::default();
        assert!(!allowed(&denied_without_rows, &principal(5), &real, &[]));
        assert!(allowed(
            &denied_without_rows,
            &principal(5),
            &AlwaysAllow,
            &[]
        ));
    }

    #[test]
    fn label_expression_alternatives_and_conjunctions() {
        // `-[:KNOWS|LIKES]->`: either alternative suffices.
        let or_plan = extract_ops(vec![expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::Or(
                Box::new(LabelExpr::Name("KNOWS".into())),
                Box::new(LabelExpr::Name("LIKES".into())),
            )),
        )]);
        let mut grants = GrantTable::default();
        grants.traverse(
            GrantSubject::Principal(principal(6)),
            Some(Direction::Outgoing),
            10,
        );
        assert!(allowed(&or_plan, &principal(6), &grants, &[]));
        let mut other = GrantTable::default();
        other.traverse(GrantSubject::Principal(principal(6)), None, 11);
        assert!(allowed(&or_plan, &principal(6), &other, &[]));
        let none = GrantTable::default();
        assert!(!allowed(&or_plan, &principal(6), &none, &[]));

        // `AND` demands both labels conjunctively.
        let and_plan = extract_ops(vec![conjunction_expand()]);
        let mut both = GrantTable::default();
        both.traverse(
            GrantSubject::Principal(principal(6)),
            Some(Direction::Outgoing),
            10,
        );
        both.traverse(GrantSubject::Principal(principal(6)), None, 11);
        assert!(allowed(&and_plan, &principal(6), &both, &[]));
        assert!(!allowed(&and_plan, &principal(6), &grants, &[]));

        // `NOT` / wildcard are not decomposable: tenancy-only.
        let not_plan = extract_ops(vec![negation_expand()]);
        assert!(not_plan.graphs.get(&GRAPH_RAW).unwrap().unattributed);
        let wildcard_plan = extract_ops(vec![wildcard_expand()]);
        assert!(wildcard_plan.graphs.get(&GRAPH_RAW).unwrap().unattributed);
    }

    fn conjunction_expand() -> PlanOp {
        expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::And(
                Box::new(LabelExpr::Name("KNOWS".into())),
                Box::new(LabelExpr::Name("LIKES".into())),
            )),
        )
    }

    fn negation_expand() -> PlanOp {
        expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::Not(Box::new(LabelExpr::Name("KNOWS".into())))),
        )
    }

    fn wildcard_expand() -> PlanOp {
        expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::Wildcard),
        )
    }

    #[test]
    fn shortest_path_demands_traversal_rows() {
        let reqs = extract_ops(vec![PlanOp::ShortestPath {
            src: "a".into(),
            dst: "b".into(),
            edge: "__sp".into(),
            path_var: None,
            emit_edge_binding: false,
            emit_path_binding: false,
            mode: gleaph_gql_planner::plan::ShortestMode::AnyShortest,
            direction: EdgeDirection::PointingRight,
            label: node_label_of_edge("KNOWS"),
            label_expr: None,
            var_len: None,
            cost: gleaph_gql_planner::plan::ShortestPathCost::HopCount,
        }]);
        let demands = reqs.graphs.get(&GRAPH_RAW).unwrap();
        assert_eq!(
            demands.conjunctive,
            vec![traverse_row(Some(Direction::Outgoing), 10)]
        );
        // Reverse shortest paths demand the reverse row.
        let reverse = extract_ops(vec![PlanOp::ShortestPath {
            src: "a".into(),
            dst: "b".into(),
            edge: "__sp".into(),
            path_var: None,
            emit_edge_binding: false,
            emit_path_binding: false,
            mode: gleaph_gql_planner::plan::ShortestMode::AnyShortest,
            direction: EdgeDirection::PointingLeft,
            label: node_label_of_edge("KNOWS"),
            label_expr: None,
            var_len: None,
            cost: gleaph_gql_planner::plan::ShortestPathCost::HopCount,
        }]);
        assert_eq!(
            reverse.graphs.get(&GRAPH_RAW).unwrap().conjunctive,
            vec![traverse_row(Some(Direction::Incoming), 10)]
        );
    }

    #[test]
    fn use_graph_child_resolves_against_the_target_graph() {
        // The child segment resolves names against graph 9's catalogs; graph-7 rows do
        // not carry over.
        let child_scan = PlanOp::NodeScan {
            variable: "n".into(),
            label: node_label("Person"),
            property_projection: projection(&[]),
        };
        let reqs = extract(
            &PhysicalPlan::from_ops(vec![PlanOp::UseGraph {
                graph_name: vec!["other".into()],
                sub_plan: Some(vec![child_scan]),
            }]),
            graph(),
            &CrossGraphCatalog,
        );
        let demands = reqs.graphs.get(&9).expect("target-graph demands");
        assert_eq!(
            demands.conjunctive,
            vec![Privilege::Graph(GraphPrivilege {
                graph: 9,
                operation: GraphOperation::Match,
                resource: GraphResource::VertexLabel(1),
            })]
        );

        struct CrossGraphCatalog;
        impl PlanCatalogView for CrossGraphCatalog {
            fn graph_by_name(&self, name: &str) -> Option<GraphId> {
                (name == "other").then(|| GraphId::from_raw(9))
            }
            fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32> {
                ((graph.raw() == 9 && name == "Person")
                    || (graph.raw() == GRAPH_RAW && name == "Person"))
                    .then_some(1)
            }
            fn edge_label(&self, _graph: GraphId, _name: &str) -> Option<(u32, Option<bool>)> {
                None
            }
            fn property(&self, _graph: GraphId, _name: &str) -> Option<u32> {
                None
            }
            fn vector_index_source(
                &self,
                _graph: GraphId,
                _index_name: &str,
            ) -> Option<VectorIndexSource> {
                None
            }
        }
    }

    #[test]
    fn unresolvable_use_graph_target_is_tenancy_only() {
        let reqs = extract_ops(vec![PlanOp::UseGraph {
            graph_name: vec!["nowhere".into()],
            sub_plan: Some(vec![scan("n", Some("Person"), &[])]),
        }]);
        assert!(reqs.root_unattributed);
        assert!(!allowed(
            &reqs,
            &principal(7),
            &grants_state_with_all_listed_rows(),
            &[]
        ));
        assert!(allowed(
            &reqs,
            &principal(7),
            &grants_state_with_all_listed_rows(),
            &[GRAPH_RAW]
        ));
    }

    // ───── ADR 0078 §1 layer 1: vector-search requirement extraction ─────

    fn vector_search_op(binding: &str, index_name: &str) -> PlanOp {
        use gleaph_gql_planner::plan::{SearchOutputKind, SearchOutputPlan, SearchProviderPlan};
        PlanOp::Search {
            binding: binding.into(),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec![Str::from(index_name)],
                query: Expr::new(ExprKind::Literal(gleaph_gql::Value::Bytes(vec![0; 4]))),
                limit: Expr::int(10),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: "distance".into(),
            },
        }
    }

    /// Catalog extended with `doc_vec`: one-label span (Person=1) whose embedding name is
    /// backed by the projected graph property age=20.
    fn catalog_with_doc_vec() -> FakeCatalog {
        let mut c = catalog();
        c.vector_indexes.push((
            "doc_vec",
            VectorIndexSource {
                labels: vec![(1, "Person".to_owned())],
                embedding_property_id: Some(20),
            },
        ));
        c
    }

    fn extract_in(catalog: &FakeCatalog, ops: Vec<PlanOp>) -> RequirementSet {
        extract(&PhysicalPlan::from_ops(ops), graph(), catalog)
    }

    fn grant_reader_of_doc_vec(subject: GrantSubject) -> GrantTable {
        let mut grants = GrantTable::default();
        grants.vertex(subject, GraphOperation::Match, 1);
        grants.vertex_property(subject, 1, 20);
        grants
    }

    #[test]
    fn vector_search_demands_scan_equivalent_rows() {
        let reqs = extract_in(
            &catalog_with_doc_vec(),
            vec![vector_search_op("d", "doc_vec")],
        );
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
                vertex_property_row(GRAPH_RAW, 1, 20),
            ]
        );
    }

    #[test]
    fn vector_search_admission_matches_row_coverage_exactly() {
        let reqs = extract_in(
            &catalog_with_doc_vec(),
            vec![vector_search_op("d", "doc_vec")],
        );
        let reader = principal(2);

        // Both rows covered -> admitted...
        assert!(allowed(
            &reqs,
            &reader,
            &grant_reader_of_doc_vec(GrantSubject::Principal(reader)),
            &[]
        ));
        // ...MATCH alone is not (the embedding read is a separate demand)...
        let mut match_only = GrantTable::default();
        match_only.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        assert!(!allowed(&reqs, &reader, &match_only, &[]));
        // ...a different label's rows are not...
        let mut wrong_label = GrantTable::default();
        wrong_label.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 2);
        wrong_label.vertex_property(GrantSubject::Principal(reader), 2, 20);
        assert!(!allowed(&reqs, &reader, &wrong_label, &[]));
        // ...and PUBLIC rows admit anonymous callers like an equivalent scan.
        assert!(allowed(
            &reqs,
            &Principal::anonymous(),
            &grant_reader_of_doc_vec(GrantSubject::Public),
            &[]
        ));

        // Wrong-implementation probe: an evaluator granting everything admits the
        // MATCH-only caller too, so the denial assertion above discriminates the real
        // evaluator from a stub.
        assert!(allowed(&reqs, &reader, &AlwaysAllow, &[]));
    }

    #[test]
    fn vector_search_multi_label_span_is_conjunctive() {
        let mut c = catalog();
        c.vector_indexes.push((
            "wide_vec",
            VectorIndexSource {
                labels: vec![(1, "Person".to_owned()), (2, "Employee".to_owned())],
                embedding_property_id: Some(21),
            },
        ));
        let reqs = extract_in(&c, vec![vector_search_op("d", "wide_vec")]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
                vertex_property_row(GRAPH_RAW, 1, 21),
                vertex_row(GRAPH_RAW, GraphOperation::Match, 2),
                vertex_property_row(GRAPH_RAW, 2, 21),
            ]
        );

        // A caller able to read only one spanned label is denied: per-candidate
        // visibility has no row-level filter, so the whole span must be readable.
        let half_reader = principal(8);
        let mut grants = GrantTable::default();
        grants.vertex(
            GrantSubject::Principal(half_reader),
            GraphOperation::Match,
            1,
        );
        grants.vertex_property(GrantSubject::Principal(half_reader), 1, 21);
        assert!(!allowed(&reqs, &half_reader, &grants, &[]));
        assert!(allowed(&reqs, &half_reader, &grants, &[GRAPH_RAW]));
    }

    #[test]
    fn vector_search_ingestion_fed_embedding_demands_no_property_row() {
        let mut c = catalog();
        c.vector_indexes.push((
            "fed_vec",
            VectorIndexSource {
                labels: vec![(1, "Person".to_owned())],
                embedding_property_id: None,
            },
        ));
        let reqs = extract_in(&c, vec![vector_search_op("d", "fed_vec")]);
        assert_eq!(
            reqs.graphs.get(&GRAPH_RAW).unwrap().conjunctive,
            vec![vertex_row(GRAPH_RAW, GraphOperation::Match, 1)]
        );
    }

    #[test]
    fn unknown_vector_index_name_is_tenancy_only() {
        let reqs = extract_in(&catalog(), vec![vector_search_op("d", "ghost_vec")]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(demands.unattributed);
        assert!(!allowed(
            &reqs,
            &principal(2),
            &grant_reader_of_doc_vec(GrantSubject::Public),
            &[]
        ));
        assert!(allowed(
            &reqs,
            &principal(2),
            &grant_reader_of_doc_vec(GrantSubject::Public),
            &[GRAPH_RAW]
        ));
    }

    // ───── ADR 0078 §6: edge-subject (GLEAPH.VECTOR.*) visibility ─────
    //
    // Edge-inline byte vectors are fused fixed-label edge predicates evaluated during
    // ordinary traversal execution. Their subjects are edges, so the identical visibility
    // rule translates to: the matching edge privilege INCLUDING DIRECTION covers the
    // inline vector read, and an invisible edge never reaches candidate evaluation.

    /// Expand hop carrying a fused `GLEAPH.VECTOR.*` edge-inline vector predicate.
    fn vector_predicate_expand(direction: EdgeDirection, label: Option<&str>) -> PlanOp {
        use gleaph_gql_planner::plan::{EdgeVectorMetric, ScanValue};
        let mut op = expand(direction, label);
        if let PlanOp::Expand {
            edge_inline_vector_predicate,
            ..
        } = &mut op
        {
            *edge_inline_vector_predicate =
                Some(gleaph_gql_planner::plan::EdgeInlineVectorPredicate {
                    metric: EdgeVectorMetric::L2Squared,
                    query: ScanValue::Parameter("q".into()),
                    op: gleaph_gql::ast::CmpOp::Le,
                    threshold: ScanValue::Literal(gleaph_gql::Value::Float32(0.5)),
                });
        }
        op
    }

    #[test]
    fn edge_inline_vector_rides_the_direction_aware_traversal_row() {
        // The fused vector predicate adds NO extra demand and grants NO bypass: the
        // demanded rows are exactly the plain traversal rows for the same direction.
        for direction in [
            EdgeDirection::PointingRight,
            EdgeDirection::PointingLeft,
            EdgeDirection::AnyDirection,
        ] {
            let plain = extract_ops(vec![expand(direction, Some("KNOWS"))]);
            let fused = extract_ops(vec![vector_predicate_expand(direction, Some("KNOWS"))]);
            assert_eq!(
                fused, plain,
                "edge-inline vector predicate must ride the traversal row ({direction:?})"
            );

            // Direction-aware coverage: a caller holding only the OUTGOING row cannot run
            // the incoming shape, so its inline edge bytes stay invisible.
            let reader = principal(11);
            let mut grants = GrantTable::default();
            grants.traverse(
                GrantSubject::Principal(reader),
                Some(Direction::Outgoing),
                10,
            );
            let outgoing = matches!(direction, EdgeDirection::PointingRight);
            assert_eq!(
                allowed(&fused, &reader, &grants, &[]),
                outgoing,
                "direction-aware coverage must gate fused vector reads ({direction:?})"
            );
            // Wrong-implementation probe: an evaluator granting every direction admits
            // the incoming shape too, so the assertion above discriminates.
            assert!(allowed(&fused, &reader, &AlwaysAllow, &[]));
        }
    }

    #[test]
    fn edge_inline_vector_without_label_stays_tenancy_only() {
        // Unlabeled fused shapes cannot be attributed to a grantable resource: tenancy
        // gates them like any other wildcard traversal.
        let fused = extract_ops(vec![vector_predicate_expand(
            EdgeDirection::PointingRight,
            None,
        )]);
        let demands = fused.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(demands.unattributed);
        assert!(!allowed(
            &fused,
            &principal(11),
            &GrantTable::default(),
            &[]
        ));
        assert!(allowed(
            &fused,
            &principal(11),
            &GrantTable::default(),
            &[GRAPH_RAW]
        ));
    }

    /// ADR 0082 §5 invariant 1: a SemiApply chain is policy machinery — its internal
    /// traversals and terminal property reads add no requirement, so a plan's demand
    /// set is identical with and without the op.
    #[test]
    fn semi_apply_chain_adds_no_requirements() {
        use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
        use gleaph_gql_planner::plan::SemiHop;

        let terminal = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable("a".to_owned()))),
                property: "age".to_owned(),
            })),
            op: CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(3)))),
        });
        let base_scan = scan("d", Some("Person"), &[]);
        let with_chain = vec![
            base_scan.clone(),
            PlanOp::SemiApply {
                source: "d".into(),
                hops: vec![SemiHop {
                    edge_label: EdgeLabelRef::new("KNOWS"),
                    direction: EdgeDirection::PointingRight,
                    dst_variable: "a".into(),
                    dst_label: NodeLabelRef::new("Employee"),
                }],
                terminal_predicates: vec![terminal],
                sub_plan: vec![expand(EdgeDirection::PointingRight, Some("KNOWS"))],
            },
        ];

        // The wrong-implementation probe shape: if the walker ever walked the chain,
        // it would demand KNOWS traversal plus Employee reads and this equality fails.
        assert_eq!(
            extract_ops(with_chain),
            extract_ops(vec![base_scan]),
            "policy-internal chain must stay invisible to requirement extraction"
        );
    }

    // ── EXPLAIN AUTHORIZATION report mode (ADR 0084) ──

    /// Fixed-name catalog for rendering tests: graph 7 = "main", Person/KNOWS/name.
    struct FixedNames;

    impl RequirementNames for FixedNames {
        fn graph_name(&self, graph_raw: u32) -> Option<String> {
            (graph_raw == GRAPH_RAW).then(|| "main".to_string())
        }
        fn vertex_label(&self, _g: u32, label_id: u32) -> Option<String> {
            (label_id == 1).then(|| "Person".to_string())
        }
        fn edge_label(&self, _g: u32, label_id: u32) -> Option<String> {
            (label_id == 10).then(|| "KNOWS".to_string())
        }
        fn property(&self, _g: u32, property_id: u32) -> Option<String> {
            (property_id == 21).then(|| "name".to_string())
        }
    }

    /// Fixture probes over explicit row sets; an absent own row models a lapsed
    /// (expired) one — the real `holds` primitive already treats expired rows as absent.
    #[derive(Debug)]
    struct FixtureProbes {
        own: Vec<Privilege>,
        public: Vec<Privilege>,
    }

    impl GrantSourceProbes for FixtureProbes {
        fn holds_own_row(&self, _s: &Principal, p: &Privilege) -> bool {
            self.own.contains(p)
        }
        fn holds_public_row(&self, p: &Privilege) -> bool {
            self.public.contains(p)
        }
    }

    /// Enforcement view of the same fixture rows: the exact `StoredGrants::covers`
    /// formula (`own || PUBLIC`), so agreement proves single semantics.
    struct ProbesAsEnforcementGrants<'a>(&'a FixtureProbes);

    impl EffectiveGrants for ProbesAsEnforcementGrants<'_> {
        fn covers(&self, caller: &Principal, privilege: &Privilege) -> bool {
            self.0.holds_own_row(caller, privilege) || self.0.holds_public_row(privilege)
        }

        fn holds_any_vertex_label_match(&self, _caller: &Principal, _graph: u32) -> bool {
            // The snapshot stores concrete rows only; the fixture's own/public lists carry
            // vertex-label MATCH rows directly, so the caller-agnostic scan suffices. The
            // `Privilege::Graph` arm of `covers` already proves per-row coverage; this
            // marker is only set on unconstrained scans where per-row evaluation is not
            // needed.
            self.0
                .own
                .iter()
                .chain(self.0.public.iter())
                .any(is_vertex_label_match)
        }
    }

    struct FakeTenancy {
        tenants: Vec<u32>,
    }

    impl GraphTenancy for FakeTenancy {
        fn is_tenant(&self, graph_raw: u32, _c: &Principal) -> bool {
            self.tenants.contains(&graph_raw)
        }
    }

    /// Whether a privilege is a vertex-label `MATCH` row covering a concrete label or
    /// the wildcard `NODES *` resource. Used by the test probe to evaluate the
    /// unconstrained-vertex-match marker.
    fn is_vertex_label_match(privilege: &Privilege) -> bool {
        matches!(
            privilege,
            Privilege::Graph(GraphPrivilege {
                operation: GraphOperation::Match,
                resource: GraphResource::VertexLabel(_) | GraphResource::AllVertexLabels,
                ..
            })
        )
    }

    fn match_person(graph: u32) -> Privilege {
        vertex_row(graph, GraphOperation::Match, 1)
    }

    fn read_person(graph: u32) -> Privilege {
        vertex_row(graph, GraphOperation::Read, 1)
    }

    fn reqs_with(rows: Vec<Privilege>, alternatives: Vec<Vec<Privilege>>) -> RequirementSet {
        let mut reqs = RequirementSet::default();
        if !rows.is_empty() {
            reqs.require_all(graph(), rows);
        }
        for group in alternatives {
            reqs.require_alternatives(graph(), vec![group]);
        }
        reqs
    }

    fn subject() -> Principal {
        Principal::from_slice(&[0xE1; 29])
    }

    #[test]
    fn report_agrees_with_authorize_requirements_on_identical_fixtures() {
        let subjects = [subject(), Principal::anonymous()];
        let grant_states = [
            FixtureProbes {
                own: vec![match_person(GRAPH_RAW), read_person(GRAPH_RAW)],
                public: vec![],
            },
            FixtureProbes {
                own: vec![],
                public: vec![match_person(GRAPH_RAW)],
            },
            FixtureProbes {
                own: vec![],
                public: vec![],
            },
            FixtureProbes {
                own: vec![read_person(GRAPH_RAW)],
                public: vec![match_person(GRAPH_RAW)],
            },
        ];
        let tenancies = [
            FakeTenancy { tenants: vec![] },
            FakeTenancy {
                tenants: vec![GRAPH_RAW],
            },
        ];
        let fixtures = [
            // Conjunctive only.
            reqs_with(vec![match_person(GRAPH_RAW)], vec![]),
            // Conjunctive + alternation.
            reqs_with(
                vec![match_person(GRAPH_RAW)],
                vec![vec![edge_row(
                    GRAPH_RAW,
                    GraphOperation::Traverse(None),
                    10,
                )]],
            ),
            // Alternation where one arm is covered by PUBLIC and the other by tenancy.
            reqs_with(
                vec![],
                vec![vec![read_person(GRAPH_RAW)], vec![match_person(GRAPH_RAW)]],
            ),
            // Unattributed residue.
            {
                let mut reqs = RequirementSet::default();
                reqs.require_unattributed(graph());
                reqs
            },
            // Root-unattributed demand.
            {
                let mut reqs = RequirementSet {
                    root_unattributed: true,
                    ..Default::default()
                };
                reqs.require_all(graph(), vec![match_person(GRAPH_RAW)]);
                reqs
            },
        ];
        for req_fixture in &fixtures {
            for grants in &grant_states {
                for tenancy in &tenancies {
                    for asker in &subjects {
                        let report = build_coverage_report(
                            req_fixture,
                            GRAPH_RAW,
                            asker,
                            tenancy,
                            grants,
                            &FixedNames,
                        );
                        let verdict = authorize_requirements(
                            req_fixture,
                            GRAPH_RAW,
                            asker,
                            &ProbesAsEnforcementGrants(grants),
                            tenancy,
                        );
                        assert_eq!(
                            report.covered, verdict,
                            "report/verdict drift on {req_fixture:?} with {grants:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn self_mode_renders_source_classes_and_owner_mode_full_identity() {
        let grants = FixtureProbes {
            own: vec![match_person(GRAPH_RAW)],
            public: vec![read_person(GRAPH_RAW)],
        };
        let reqs = reqs_with(
            vec![match_person(GRAPH_RAW), read_person(GRAPH_RAW)],
            vec![],
        );
        let no_tenancy = FakeTenancy { tenants: vec![] };
        let report = build_coverage_report(
            &reqs,
            GRAPH_RAW,
            &subject(),
            &no_tenancy,
            &grants,
            &FixedNames,
        );
        let self_lines = render_coverage_report(&report, ExplainRenderMode::SelfMode, "ignored");
        assert_eq!(self_lines[0], "COVERED");
        assert!(self_lines.contains(&"  MATCH ON NODES Person — your grant".to_string()));
        assert!(self_lines.contains(&"  READ ON NODES Person — PUBLIC grant".to_string()));
        // No foreign principal names exist anywhere in self-mode output.
        let owner_text = subject().to_text();
        for line in &self_lines {
            assert!(
                !line.contains("grant to"),
                "self mode must not name principals: {line}"
            );
            assert!(!line.contains(&owner_text));
        }

        let owner_lines =
            render_coverage_report(&report, ExplainRenderMode::OwnerMode, &owner_text);
        assert!(owner_lines.contains(&format!("  MATCH ON NODES Person — grant to {owner_text}")));
    }

    #[test]
    fn alternatives_unattributed_and_metadata_render_per_adr() {
        let mut reqs = reqs_with(
            vec![vertex_property_row(GRAPH_RAW, 1, 21)],
            vec![vec![
                edge_row(
                    GRAPH_RAW,
                    GraphOperation::Traverse(Some(Direction::Outgoing)),
                    10,
                ),
                edge_row(
                    GRAPH_RAW,
                    GraphOperation::Traverse(Some(Direction::Incoming)),
                    10,
                ),
            ]],
        );
        reqs.require(
            graph(),
            Privilege::Metadata(MetadataScope::Graph(GRAPH_RAW)),
        );
        reqs.require_unattributed(graph());
        let grants = FixtureProbes {
            own: vec![vertex_property_row(GRAPH_RAW, 1, 21)],
            public: vec![edge_row(
                GRAPH_RAW,
                GraphOperation::Traverse(Some(Direction::Incoming)),
                10,
            )],
        };
        let report = build_coverage_report(
            &reqs,
            GRAPH_RAW,
            &subject(),
            &FakeTenancy { tenants: vec![] },
            &grants,
            &FixedNames,
        );
        assert!(!report.covered);
        let lines = render_coverage_report(&report, ExplainRenderMode::SelfMode, "x");
        assert!(lines.contains(&"  READ ON NODES Person { name } — your grant".to_string()));
        assert!(lines.contains(&"  ANY OF:".to_string()));
        assert!(lines.contains(&"    TRAVERSE OUTGOING ON EDGES KNOWS — UNCOVERED".to_string()));
        assert!(lines.contains(&"    TRAVERSE INCOMING ON EDGES KNOWS — PUBLIC grant".to_string()));
        assert!(
            lines.contains(&"  UNATTRIBUTED READ — requires graph tenancy (owner/admin)".into())
        );
        assert!(lines.contains(&"  READ_METADATA ON GRAPH main — UNCOVERED".to_string()));
    }

    #[test]
    fn expired_rows_are_absent_from_reports() {
        // The production probe is `holds`, which already reads expiry; model its
        // observable effect: an expired own row covers nothing.
        let grants = FixtureProbes {
            own: vec![],
            public: vec![],
        };
        let reqs = reqs_with(vec![match_person(GRAPH_RAW)], vec![]);
        let report = build_coverage_report(
            &reqs,
            GRAPH_RAW,
            &subject(),
            &FakeTenancy { tenants: vec![] },
            &grants,
            &FixedNames,
        );
        assert!(!report.covered);
        assert_eq!(
            report.graphs[0].conjunctive[0].source, None,
            "an expired row must render uncovered, never as a source"
        );
    }

    #[test]
    fn explain_visibility_gate_requires_every_graph_and_ignores_caps() {
        let asker = subject();
        let may_access = |g: u32, _a: &Principal| g == GRAPH_RAW;
        let tenant_of = |g: u32, a: &Principal| g == GRAPH_RAW && *a == asker;

        // Self mode: all visible graphs pass.
        authorize_explain_visibility(vec![GRAPH_RAW], &asker, false, &may_access, &tenant_of)
            .expect("visible graph passes self gate");

        // Self mode: one invisible touched graph fails with NotFound.
        let err = authorize_explain_visibility(
            vec![GRAPH_RAW, 99],
            &asker,
            false,
            &may_access,
            &tenant_of,
        )
        .expect_err("invisible graph must fail");
        assert!(matches!(err, RouterError::NotFound(_)));

        // Owner mode: visibility is not enough — registry tenancy is required even of
        // a grant-derived visible principal who is not a tenant.
        let err = authorize_explain_visibility(
            vec![GRAPH_RAW],
            &Principal::from_slice(&[0xE2; 29]),
            true,
            &may_access,
            &tenant_of,
        )
        .expect_err("owner mode requires tenancy");
        assert!(matches!(err, RouterError::NotFound(_)));

        // Structural caps proof: the gate signature takes no capability input, so a
        // MANAGE_AUTHORIZATION holder is indistinguishable from a stranger here.
    }

    /// Full entry on live Router stable state: record resolution, gate ordering, and
    /// the uniform-NotFound contract ([ADR 0084] §3/§6 invariant 4).
    #[test]
    fn explain_entry_gates_before_rendering_with_uniform_not_found() {
        use crate::facade::stable::prepared_catalog::{
            PreparedPlanKey, PreparedPlanRecord, PreparedPlanRecordV1, insert_prepared_plan,
        };
        use crate::facade::store::tests::{register_test_graph, test_init_args};

        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let owner = Principal::from_slice(&[0x0A; 29]);
        crate::facade::auth::grant_admins(&[owner]);
        register_test_graph(&store, owner, "explain.main");
        let graph_id = crate::facade::stable::graph_catalog::lookup_graph_id("explain.main")
            .expect("registered graph");

        // Prepared record with one conjunctive MATCH demand on label 1 of its graph.
        let mut reqs = RequirementSet::default();
        reqs.require(graph_id, match_person(graph_id.raw()));
        insert_prepared_plan(
            PreparedPlanKey::new("adr0084_q"),
            PreparedPlanRecord::V1(PreparedPlanRecordV1 {
                graph_id,
                query: "MATCH (p:Person) RETURN p".to_string(),
                metadata: None,
                required_privileges: reqs,
            }),
        );

        let stranger = Principal::from_slice(&[0xE3; 29]);
        let missing =
            explain_prepared_authorization(&store, &stranger, "adr0084_absent", &stranger, false)
                .expect_err("unknown record");
        let gated =
            explain_prepared_authorization(&store, &stranger, "adr0084_q", &stranger, false)
                .expect_err("invisible touched graph");
        assert!(
            matches!(missing, RouterError::NotFound(_))
                && matches!(gated, RouterError::NotFound(_)),
            "both failures are NotFound"
        );
        // Existence-oracle dead end: each failure carries exactly the asked name and
        // nothing else, so "absent" and "present but invisible" have identical shapes.
        assert_eq!(
            format!("{missing:?}"),
            r#"NotFound("prepared query \"adr0084_absent\"")"#
        );
        assert_eq!(
            format!("{gated:?}"),
            r#"NotFound("prepared query \"adr0084_q\"")"#
        );

        // A full-caps holder without visibility receives the identical NotFound:
        // capabilities confer no explain authority (ADR 0074 invariant 1).
        crate::facade::auth::grant_admins(&[Principal::from_slice(&[0xE4; 29])]);
        let caps_holder = explain_prepared_authorization(
            &store,
            &Principal::from_slice(&[0xE4; 29]),
            "adr0084_q",
            &stranger,
            true,
        )
        .expect_err("caps holder in owner mode");
        assert_eq!(format!("{caps_holder:?}"), format!("{gated:?}"));

        // A grantee passes the settled visibility arms and gets rendered coverage; no
        // rendering happened above because every rejection preceded it.
        let grantee = subject();
        crate::facade::auth::add_grant(
            GrantSubject::effective_for(&grantee),
            &match_person(graph_id.raw()),
            None,
            None,
        )
        .expect("grant row");
        let lines = explain_prepared_authorization(&store, &grantee, "adr0084_q", &grantee, false)
            .expect("grantee self-explain");
        assert_eq!(lines[0], "COVERED");
        assert!(lines.iter().any(|l| l.ends_with("— your grant")));

        // Owner mode by the tenant renders the grantee's full identity.
        let owner_lines =
            explain_prepared_authorization(&store, &owner, "adr0084_q", &grantee, true)
                .expect("owner explains collaborator");
        assert_eq!(owner_lines[0], "COVERED");
        assert!(
            owner_lines
                .iter()
                .any(|l| l.contains(&format!("grant to {}", grantee.to_text())))
        );

        // Owner mode refused to a non-tenant asker, indistinguishably.
        let non_tenant_owner_ask =
            explain_prepared_authorization(&store, &stranger, "adr0084_q", &grantee, true)
                .expect_err("BY requires tenancy of every touched graph");
        assert!(matches!(non_tenant_owner_ask, RouterError::NotFound(_)));
    }
}
