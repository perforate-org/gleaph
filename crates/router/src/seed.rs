//! Index anchor detection and per-shard seed binding for graph dispatch.
//!
//! The router resolves query entry points via graph-index before calling graph shards.
//! [`IndexAnchor`] captures the leading anchor op (`IndexScan`, `IndexIntersection`, or
//! labeled `NodeScan`); hits are encoded into `seed_bindings_blob` so shards can skip
//! that op.

use std::collections::{BTreeMap, BTreeSet};

use candid::Encode;
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql::value_to_index_key_bytes;
use gleaph_gql_planner::GraphStats;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::plan::{PlanOp, ScanValue, Str};
use gleaph_graph_kernel::entry::{GraphId, PropertyId};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingHit, IndexEqualSpec, PhysicalIndexId, PostingHit, validate_index_value_key_bytes,
};
use gleaph_graph_kernel::plan_exec::{LocalEdgePosting, SeedBindingEntry, SeedBindingsWire};

use crate::edge_index_direction::wire_labels_for_query;
use crate::facade::stable::indexed_catalog;
use crate::facade::store::RouterStore;
use crate::planner_stats::RouterGraphStats;
use crate::state::RouterError;

/// Index lookup anchor extracted from a physical plan.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IndexAnchor {
    /// Single equality `IndexScan` (`lookup_equal`).
    Equal(SeedProbe),
    /// Union of equality `IndexScan` point probes over one property
    /// (`WHERE v.prop IN (…)` anchor; per-probe `lookup_equal`, merged with
    /// global deduplication).
    EqualUnion {
        variable: String,
        probes: Vec<SeedProbe>,
    },
    /// Leading `EdgeIndexScan` (`lookup_edge_equal`).
    EdgeEqual(EdgeSeedProbe),
    /// Union of equality `EdgeIndexScan` point probes over one property
    /// (`WHERE e.prop IN (…)` anchor; per-probe `lookup_edge_equal`, merged
    /// with global edge-identity deduplication).
    EdgeEqualUnion {
        variable: String,
        probes: Vec<EdgeSeedProbe>,
    },
    /// Single range `IndexScan` bound (`lookup_range`, one-sided only).
    Range(RangeSeedProbe),
    /// Text prefix `IndexScan` bound (`lookup_range`, encoded TEXT prefix interval).
    Prefix(PrefixSeedProbe),
    /// Leading one-sided range `EdgeIndexScan` (`lookup_edge_range`, domain-clamped at execution).
    EdgeRange(EdgeRangeSeedProbe),
    /// Text prefix `EdgeIndexScan` bound (`lookup_edge_range`, encoded TEXT prefix interval).
    EdgePrefix(EdgePrefixSeedProbe),
    /// Multiple equality arms (`lookup_intersection`).
    Intersection {
        variable: String,
        specs: Vec<IndexEqualSpec>,
    },
    /// Labeled `NodeScan` (paginated `lookup_label_page` per shard).
    Label {
        variable: String,
        vertex_label_id: u32,
    },
    /// Multi-label `NodeScan` + `IsLabeled` filters (paginated walk + label sieve).
    LabelIntersection {
        variable: String,
        vertex_label_ids: Vec<u32>,
    },
}

impl IndexAnchor {
    /// Variable bound by this anchor (also used in `SeedBindingsWire`).
    pub fn variable(&self) -> &str {
        match self {
            Self::Equal(probe) => probe.variable.as_str(),
            Self::EqualUnion { variable, .. } => variable.as_str(),
            Self::EdgeEqual(probe) => probe.variable.as_str(),
            Self::EdgeEqualUnion { variable, .. } => variable.as_str(),
            Self::Range(probe) => probe.variable.as_str(),
            Self::Prefix(probe) => probe.variable.as_str(),
            Self::EdgeRange(probe) => probe.variable.as_str(),
            Self::EdgePrefix(probe) => probe.variable.as_str(),
            Self::Intersection { variable, .. } => variable.as_str(),
            Self::Label { variable, .. } => variable.as_str(),
            Self::LabelIntersection { variable, .. } => variable.as_str(),
        }
    }

    /// Scan physical plans for the first index anchor (`IndexIntersection`, equality `IndexScan`, or labeled `NodeScan`).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "convenience wrapper over SeedAnchorSet::from_plans"
        )
    )]
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
        graph_id: GraphId,
    ) -> Result<Option<Self>, RouterError> {
        let stats = RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
        );
        Ok(SeedAnchorSet::from_plans(plans, parameters, store, &stats)?
            .map(|set| set.routing_anchor()))
    }
}

/// One or more index/label anchors for one variable in a multi-variable seed prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariableAnchor {
    pub variable: String,
    pub anchors: Vec<IndexAnchor>,
}

impl VariableAnchor {
    pub fn routing_anchor(&self) -> IndexAnchor {
        self.anchors
            .first()
            .expect("VariableAnchor has at least one anchor")
            .clone()
    }
}

/// One or more index/label anchors on the same variable for router seed routing.
///
/// ADR 0046 Phase 1: expanded to represent multiple independently anchored variables in the
/// leading read prefix. Single-variable callers continue to use the first variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedAnchorSet {
    pub variables: Vec<VariableAnchor>,
}

fn has_non_label_anchor(anchor: &VariableAnchor) -> bool {
    anchor.anchors.iter().any(|a| {
        !matches!(
            a,
            IndexAnchor::Label { .. } | IndexAnchor::LabelIntersection { .. }
        )
    })
}

impl SeedAnchorSet {
    /// Leading anchor prefix for seed routing (label + property intersection when present).
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
        stats: &RouterGraphStats,
    ) -> Result<Option<Self>, RouterError> {
        let mut all: Vec<VariableAnchor> = Vec::new();
        for plan in plans {
            if let Some(prefix) =
                parse_seed_anchor_prefix_multi(&plan.ops, parameters, store, stats)?
            {
                all.extend(prefix);
            }
        }
        if all.is_empty() {
            return Ok(None);
        }
        // ADR 0046 Phase 1: a multi-variable prefix is only useful when every variable has at
        // least one selective equality/index anchor. Label-only variables would explode the
        // Cartesian product and provide no index path, so fall back to Graph-local execution.
        if all.len() > 1 && !all.iter().all(has_non_label_anchor) {
            return Ok(None);
        }
        Ok(Some(Self { variables: all }))
    }

    /// Variable of the first anchored variable (backward-compatible accessor).
    #[cfg_attr(not(test), expect(dead_code, reason = "backward-compatible accessor"))]
    pub fn variable(&self) -> &str {
        self.variables[0].variable.as_str()
    }

    /// Anchors of the first anchored variable (backward-compatible accessor).
    pub fn anchors(&self) -> &[IndexAnchor] {
        &self.variables[0].anchors
    }

    /// Representative anchor for [`crate::federation::SeedRouting`].
    pub fn routing_anchor(&self) -> IndexAnchor {
        self.variables[0].routing_anchor()
    }
}

/// Equality `IndexScan` anchor (one property lookup via `lookup_equal`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SeedProbe {
    /// GQL variable to seed (e.g. `"u"` in `MATCH (u {uid: $x})`).
    pub variable: String,
    /// Property name from the plan (router catalog lookup).
    pub property: String,
    /// Interned property id for index canister calls.
    pub property_id: u32,
    /// Router-owned physical posting namespace selected from one Active catalog row.
    pub physical_index_id: PhysicalIndexId,
    /// Index key bytes for `lookup_equal` (`value_to_index_key_bytes` encoding).
    pub payload_bytes: Vec<u8>,
}

/// One-sided ordered range bound carried by a range seed probe.
///
/// A dedicated enum (instead of [`CmpOp`]) keeps `Eq`/`Ne` out of the probe's valid
/// state space and preserves `Hash` on anchor cache keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeedRangeBound {
    /// Strictly below the encoded bound.
    Lt,
    /// At or below the encoded bound.
    Le,
    /// Strictly above the encoded bound.
    Gt,
    /// At or above the encoded bound.
    Ge,
}

impl SeedRangeBound {
    /// `Some` only for one-sided comparisons; `Eq` and `Ne` are not range bounds.
    pub fn try_from_cmp(cmp: CmpOp) -> Option<Self> {
        match cmp {
            CmpOp::Lt => Some(Self::Lt),
            CmpOp::Le => Some(Self::Le),
            CmpOp::Gt => Some(Self::Gt),
            CmpOp::Ge => Some(Self::Ge),
            CmpOp::Eq | CmpOp::Ne => None,
        }
    }

    /// The equivalent one-sided [`CmpOp`] for comparison-domain bound derivation.
    pub fn to_cmp(self) -> CmpOp {
        match self {
            Self::Lt => CmpOp::Lt,
            Self::Le => CmpOp::Le,
            Self::Gt => CmpOp::Gt,
            Self::Ge => CmpOp::Ge,
        }
    }
}

/// Ordered range `IndexScan` anchor (`lookup_range`, one-sided bound).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RangeSeedProbe {
    /// GQL variable to seed.
    pub variable: String,
    /// Property name from the plan (router catalog lookup).
    pub property: String,
    /// Interned property id for index canister calls.
    pub property_id: u32,
    /// Router-owned physical posting namespace selected from one Active catalog row.
    pub physical_index_id: PhysicalIndexId,
    /// Encoded bound bytes (`value_to_index_key_bytes` encoding).
    pub bound_bytes: Vec<u8>,
    /// Bound comparison direction.
    pub bound: SeedRangeBound,
}

/// Text prefix `IndexScan` anchor (`lookup_range` over the encoded TEXT prefix interval).
///
/// Carries the fully encoded pattern key (`[TEXT tag] + escaped pattern + [0, 0]`); the
/// half-open prefix interval is derived at collection time through the shared
/// `gql::text_prefix_range_bounds_for_encoded_key` helper, keeping encoded-key knowledge in
/// `gql::value_index_key`. The struct stays `Hash`-stable for anchor cache keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrefixSeedProbe {
    /// GQL variable to seed.
    pub variable: String,
    /// Property name from the plan (router catalog lookup).
    pub property: String,
    /// Interned property id for index canister calls.
    pub property_id: u32,
    /// Router-owned physical posting namespace selected from one Active catalog row.
    pub physical_index_id: PhysicalIndexId,
    /// Encoded TEXT pattern key including its `[0, 0]` terminator.
    pub pattern_bytes: Vec<u8>,
}

/// Equality `EdgeIndexScan` anchor (`lookup_edge_equal`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EdgeSeedProbe {
    pub variable: String,
    pub property: String,
    pub property_id: u32,
    /// Router-owned physical posting namespace selected from one Active catalog row.
    pub physical_index_id: PhysicalIndexId,
    pub payload_bytes: Vec<u8>,
    /// Wire label ids to scan in graph-index (ADR 0012); empty means unfiltered.
    pub wire_label_ids: Vec<u16>,
}

/// One-sided ordered range `EdgeIndexScan` anchor (`lookup_edge_range`).
///
/// Carries the encoded bound plus its comparison direction; the comparison-domain interval is
/// derived at collection time through the shared `gql::range_bounds` helper (the same drain+clamp
/// contract as [`RangeSeedProbe`]). The struct stays `Hash`-stable for anchor cache keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EdgeRangeSeedProbe {
    pub variable: String,
    pub property: String,
    pub property_id: u32,
    /// Router-owned physical posting namespace selected from one Active catalog row.
    pub physical_index_id: PhysicalIndexId,
    /// Encoded bound bytes (`value_to_index_key_bytes` encoding).
    pub bound_bytes: Vec<u8>,
    /// Bound comparison direction.
    pub bound: SeedRangeBound,
    /// Wire label ids to scan in graph-index (ADR 0012); empty means unfiltered.
    pub wire_label_ids: Vec<u16>,
}

/// Text prefix `EdgeIndexScan` anchor (`lookup_edge_range`, encoded TEXT prefix interval).
///
/// Carries the stored pattern key; the half-open interval is derived at collection time
/// through the shared `gql::text_prefix_range_bounds_for_encoded_key` helper (the same
/// SSOT contract as [`PrefixSeedProbe`]). The struct stays `Hash`-stable for cache keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EdgePrefixSeedProbe {
    pub variable: String,
    pub property: String,
    pub property_id: u32,
    /// Router-owned physical posting namespace selected from one Active catalog row.
    pub physical_index_id: PhysicalIndexId,
    /// Encoded TEXT pattern key including its `[0, 0]` terminator.
    pub pattern_bytes: Vec<u8>,
    /// Wire label ids to scan in graph-index (ADR 0012); empty means unfiltered.
    pub wire_label_ids: Vec<u16>,
}

impl SeedProbe {
    /// Returns `Some` only when the anchor is a single equality `IndexScan`.
    #[cfg_attr(not(test), expect(dead_code, reason = "test and tooling helper"))]
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
        graph_id: GraphId,
    ) -> Result<Option<Self>, RouterError> {
        Ok(
            match IndexAnchor::from_plans(plans, parameters, store, graph_id)? {
                Some(IndexAnchor::Equal(probe)) => Some(probe),
                Some(IndexAnchor::EqualUnion { .. })
                | Some(IndexAnchor::Range(_))
                | Some(IndexAnchor::Prefix(_))
                | Some(IndexAnchor::Intersection { .. })
                | Some(IndexAnchor::EdgeEqual(_))
                | Some(IndexAnchor::EdgeEqualUnion { .. })
                | Some(IndexAnchor::EdgeRange(_))
                | Some(IndexAnchor::EdgePrefix(_))
                | Some(IndexAnchor::Label { .. })
                | Some(IndexAnchor::LabelIntersection { .. })
                | None => None,
            },
        )
    }
}

/// Extract an index anchor from a single-op prefix (`IndexScan` or `IndexIntersection`).
pub(crate) fn index_anchor_from_prefix_ops(
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    store: &RouterStore,
    graph_id: GraphId,
) -> Result<Option<IndexAnchor>, RouterError> {
    match ops {
        [] => Ok(None),
        [op] => extract_from_op(op, parameters, store, graph_id),
        [
            PlanOp::EdgeIndexScan {
                variable,
                property,
                value,
                cmp,
                ..
            },
            PlanOp::EdgeBindEndpoints {
                label, direction, ..
            },
        ] => {
            let edge_label = label.as_ref().map(|l| l.as_ref());
            let query_direction = Some(*direction);
            // The prefix arm must precede the equality arm: a TextPrefix bound
            // rides `cmp = Eq` and would otherwise reach `resolve_scan_value`
            // unexpanded, which is a malformed-plan rejection.
            if let ScanValue::TextPrefix(pattern) = value {
                return edge_prefix_anchor(
                    store,
                    graph_id,
                    parameters,
                    variable,
                    property,
                    pattern,
                    edge_label,
                    query_direction,
                );
            }
            match cmp {
                CmpOp::Eq => Ok(Some(match value {
                    ScanValue::InList(elements) => edge_equal_union_anchor(
                        store,
                        graph_id,
                        parameters,
                        variable,
                        property,
                        elements,
                        edge_label,
                        query_direction,
                    )?,
                    single => edge_equal_anchor(
                        store,
                        graph_id,
                        parameters,
                        variable,
                        property,
                        single,
                        edge_label,
                        query_direction,
                    )?,
                })),
                CmpOp::Ne => Ok(None),
                _ => edge_range_anchor(
                    store,
                    graph_id,
                    parameters,
                    variable,
                    property.as_ref(),
                    value,
                    *cmp,
                    edge_label,
                    query_direction,
                ),
            }
        }
        _ => Ok(None),
    }
}

fn resolve_vertex_label_id(
    store: &RouterStore,
    graph_id: GraphId,
    label: &str,
) -> Result<u32, RouterError> {
    Ok(u32::from(
        store
            .lookup_vertex_label_id(graph_id, label)
            .map_err(|_| RouterError::NotFound(format!("label {label}")))?
            .raw(),
    ))
}

/// Catalog context every edge equality seed probe shares: the interned property
/// id, the Active physical posting namespace, and the ADR 0012 wire-label subset.
struct EdgeEqualSeedContext {
    property_id: u32,
    physical_index_id: PhysicalIndexId,
    wire_label_ids: Vec<u16>,
}

fn resolve_edge_equal_seed_context(
    store: &RouterStore,
    graph_id: GraphId,
    property: &str,
    edge_label: Option<&str>,
    query_direction: Option<EdgeDirection>,
) -> Result<EdgeEqualSeedContext, RouterError> {
    let property_id = store
        .lookup_property_id(graph_id, property)
        .map_err(|_| RouterError::NotFound(format!("property {property}")))?
        .raw();
    let edge_label_catalog = edge_label
        .map(|label| {
            store
                .lookup_edge_label_id(graph_id, label)
                .map_err(|_| RouterError::NotFound(format!("edge label {label}")))
        })
        .transpose()?;
    let physical_index_id = indexed_catalog::active_edge_physical_index(
        graph_id,
        edge_label_catalog.map(|id| id.raw()),
        PropertyId::from_raw(property_id),
        query_direction,
    )?;
    let wire_label_ids = match (edge_label_catalog, query_direction) {
        (Some(catalog), Some(direction)) => wire_labels_for_query(catalog, direction),
        _ => Vec::new(),
    };
    Ok(EdgeEqualSeedContext {
        property_id,
        physical_index_id,
        wire_label_ids,
    })
}

fn edge_equal_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: impl AsRef<str>,
    property: impl AsRef<str>,
    value: &ScanValue,
    edge_label: Option<&str>,
    query_direction: Option<EdgeDirection>,
) -> Result<IndexAnchor, RouterError> {
    let payload_bytes = resolve_scan_value(value, params)?
        .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
    let ctx = resolve_edge_equal_seed_context(
        store,
        graph_id,
        property.as_ref(),
        edge_label,
        query_direction,
    )?;
    Ok(IndexAnchor::EdgeEqual(EdgeSeedProbe {
        variable: variable.as_ref().to_string(),
        property: property.as_ref().to_string(),
        property_id: ctx.property_id,
        physical_index_id: ctx.physical_index_id,
        payload_bytes,
        wire_label_ids: ctx.wire_label_ids,
    }))
}

/// Build the union-of-point-probes anchor for a leading equality `EdgeIndexScan`
/// whose bound is [`ScanValue::InList`] (the edge-side twin of [`equal_union_anchor`]).
///
/// Each element resolves independently; Null elements contribute no probe because
/// no posting can equal them, while unencodable or missing-parameter elements fail
/// closed through [`resolve_scan_value`]. An empty probe list is kept: the union of
/// nothing matches zero rows, which mirrors `e.prop IN []` semantics. The union is
/// built before [`resolve_scan_value`] can see an `InList` bound, keeping that arm
/// a malformed-plan rejection.
fn edge_equal_union_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: &Str,
    property: &Str,
    elements: &[ScanValue],
    edge_label: Option<&str>,
    query_direction: Option<EdgeDirection>,
) -> Result<IndexAnchor, RouterError> {
    let ctx = resolve_edge_equal_seed_context(
        store,
        graph_id,
        property.as_ref(),
        edge_label,
        query_direction,
    )?;
    let mut probes = Vec::with_capacity(elements.len());
    for element in elements {
        let Some(payload_bytes) = resolve_scan_value(element, params)? else {
            continue;
        };
        probes.push(EdgeSeedProbe {
            variable: variable.as_ref().to_string(),
            property: property.as_ref().to_string(),
            property_id: ctx.property_id,
            physical_index_id: ctx.physical_index_id,
            payload_bytes,
            wire_label_ids: ctx.wire_label_ids.clone(),
        });
    }
    Ok(IndexAnchor::EdgeEqualUnion {
        variable: variable.as_ref().to_string(),
        probes,
    })
}

fn variable_from_expr(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name.as_str()),
        _ => None,
    }
}

/// Build an edge range anchor from a leading non-Eq `EdgeIndexScan` bound.
///
/// Returns `Ok(None)` when the literal's comparison domain cannot form a domain-clamped
/// interval (Bool/List/Record/Path/Extension/Duration): such predicates cannot seed a
/// shard and must be answered by the non-index filter path instead.
#[allow(clippy::too_many_arguments)]
fn edge_range_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: impl AsRef<str>,
    property: impl AsRef<str>,
    value: &ScanValue,
    cmp: CmpOp,
    edge_label: Option<&str>,
    query_direction: Option<EdgeDirection>,
) -> Result<Option<IndexAnchor>, RouterError> {
    let Some(bound) = SeedRangeBound::try_from_cmp(cmp) else {
        return Err(RouterError::InvalidArgument(format!(
            "edge range seed anchor requires a one-sided comparison, got {cmp:?}"
        )));
    };
    let bound_bytes = resolve_scan_value(value, params)?
        .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
    if gleaph_gql::value_index_key::range_bounds_for_encoded_key(&bound_bytes, bound.to_cmp())
        .is_err()
    {
        return Ok(None);
    }
    let property_id = store
        .lookup_property_id(graph_id, property.as_ref())
        .map_err(|_| RouterError::NotFound(format!("property {}", property.as_ref())))?
        .raw();
    let edge_label_id = edge_label
        .map(|label| {
            store
                .lookup_edge_label_id(graph_id, label)
                .map(|id| id.raw())
                .map_err(|_| RouterError::NotFound(format!("edge label {label}")))
        })
        .transpose()?;
    let physical_index_id = indexed_catalog::active_edge_physical_index(
        graph_id,
        edge_label_id,
        PropertyId::from_raw(property_id),
        query_direction,
    )?;
    let wire_label_ids = match (edge_label, query_direction) {
        (Some(label), Some(direction)) => {
            let catalog = store
                .lookup_edge_label_id(graph_id, label)
                .map_err(|_| RouterError::NotFound(format!("edge label {label}")))?;
            wire_labels_for_query(catalog, direction)
        }
        _ => Vec::new(),
    };
    Ok(Some(IndexAnchor::EdgeRange(EdgeRangeSeedProbe {
        variable: variable.as_ref().to_string(),
        property: property.as_ref().to_string(),
        property_id,
        physical_index_id,
        bound_bytes,
        bound,
        wire_label_ids,
    })))
}

/// Build an edge text-prefix anchor from a leading `EdgeIndexScan` whose bound is
/// [`ScanValue::TextPrefix`].
///
/// Returns `Ok(None)` when the resolved pattern is Null or cannot form a TEXT
/// prefix interval (non-TEXT domain, malformed encoded key): such predicates
/// seed no rows through the prefix path and must be answered by the non-index
/// filter path instead. Catalog resolution reuses the shared edge seed context.
fn edge_prefix_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: impl AsRef<str>,
    property: impl AsRef<str>,
    pattern: &ScanValue,
    edge_label: Option<&str>,
    query_direction: Option<EdgeDirection>,
) -> Result<Option<IndexAnchor>, RouterError> {
    let Some(pattern_bytes) = resolve_scan_value(pattern, params)? else {
        // A Null pattern matches no row under three-valued logic; the residual
        // filter path answers the predicate without a seed.
        return Ok(None);
    };
    if gleaph_gql::value_index_key::text_prefix_range_bounds_for_encoded_key(&pattern_bytes)
        .is_err()
    {
        return Ok(None);
    }
    let ctx = resolve_edge_equal_seed_context(
        store,
        graph_id,
        property.as_ref(),
        edge_label,
        query_direction,
    )?;
    Ok(Some(IndexAnchor::EdgePrefix(EdgePrefixSeedProbe {
        variable: variable.as_ref().to_string(),
        property: property.as_ref().to_string(),
        property_id: ctx.property_id,
        physical_index_id: ctx.physical_index_id,
        pattern_bytes,
        wire_label_ids: ctx.wire_label_ids,
    })))
}

/// Leading plan ops that establish index/label membership for one variable.
/// Parse a leading read prefix into one `VariableAnchor` per independently anchored
/// variable. Only contiguous leading anchor ops on distinct variables are extracted; the first
/// non-anchor op or the first unsupported op stops parsing for that variable group.
///
/// ADR 0046 Phase 1: the implementation is intentionally conservative. Correlated predicates,
/// optional bindings, and unsupported joins stop extraction so the caller falls back to Graph-local
/// execution rather than emitting a partial or wrong seed relation.
pub(crate) fn parse_seed_anchor_prefix_multi(
    ops: &[PlanOp],
    params: &BTreeMap<String, Value>,
    store: &RouterStore,
    stats: &RouterGraphStats,
) -> Result<Option<Vec<VariableAnchor>>, RouterError> {
    let mut variables: Vec<VariableAnchor> = Vec::new();
    let mut i = 0usize;
    while i < ops.len() {
        // ADR 0046 Phase 1: disconnected MATCH paths are planned as a CartesianProduct of
        // independent leading scans. Recurse into each arm so the Router can still resolve a
        // complete-row seed relation across the disconnected variables.
        if let PlanOp::CartesianProduct { left, right } = &ops[i] {
            let left_vars = parse_seed_anchor_prefix_multi(left, params, store, stats)?;
            let right_vars = parse_seed_anchor_prefix_multi(right, params, store, stats)?;
            if let (Some(left_vars), Some(right_vars)) = (left_vars, right_vars) {
                let mut seen: std::collections::BTreeSet<String> =
                    variables.iter().map(|v| v.variable.clone()).collect();
                let mut added_any = false;
                // ADR 0046: CartesianProduct arms are expected to be variable-disjoint.
                // A duplicate variable means the planner produced an unsupported shape; stop
                // extraction so the caller falls back to Graph-local execution instead of
                // merging potentially conflicting anchors for the same variable.
                for v in left_vars.into_iter().chain(right_vars) {
                    if !seen.insert(v.variable.clone()) {
                        break;
                    }
                    variables.push(v);
                    added_any = true;
                }
                if added_any {
                    i += 1;
                    continue;
                }
            }
            break;
        }

        // Try to extract the next variable's anchor prefix starting at `i`.
        let prefix = parse_seed_anchor_prefix_single(&ops[i..], params, store, stats)?;
        let Some(prefix) = prefix else {
            break;
        };
        if prefix.is_empty() {
            break;
        }
        let variable = prefix[0].variable().to_string();
        // Reject duplicate variables in the prefix.
        if variables.iter().any(|v| v.variable == variable) {
            break;
        }
        // Advance by the number of ops consumed for this variable. We approximate by walking
        // forward while ops still belong to the same variable or are skippable structural ops.
        let consumed = consumed_anchor_ops(&ops[i..], &variable);
        variables.push(VariableAnchor {
            variable,
            anchors: prefix,
        });
        i += consumed.max(1);
    }
    if variables.is_empty() {
        Ok(None)
    } else {
        Ok(Some(variables))
    }
}

/// Count how many leading ops of `ops` were consumed by the anchor prefix for `variable`.
/// Stops at the first op that either belongs to a different anchored variable or is not a
/// seed-skippable structural op (`PropertyFilter` on the same variable).
fn consumed_anchor_ops(ops: &[PlanOp], variable: &str) -> usize {
    let mut i = 0usize;
    while i < ops.len() {
        match &ops[i] {
            PlanOp::NodeScan {
                label: Some(_),
                variable: v,
                ..
            } if v.as_ref() == variable => i += 1,
            PlanOp::IndexScan { variable: v, .. } if v.as_ref() == variable => i += 1,
            PlanOp::IndexIntersection { variable: v, .. } if v.as_ref() == variable => i += 1,
            PlanOp::EdgeIndexScan { variable: v, .. } if v.as_ref() == variable => {
                // EdgeIndexScan is always the last anchor op for its variable.
                return i
                    + 1
                    + usize::from(
                        ops.get(i + 1)
                            .is_some_and(|op| matches!(op, PlanOp::EdgeBindEndpoints { .. })),
                    );
            }
            PlanOp::PropertyFilter { predicates, .. } => {
                // A PropertyFilter is part of the anchor prefix only when every predicate
                // refers to the current variable and strengthens an existing anchor.
                let all_same_var = predicates.iter().all(|pred| {
                    anchor_from_property_predicate_target_variable(pred) == Some(variable)
                });
                if all_same_var {
                    i += 1;
                } else {
                    return i;
                }
            }
            _ => return i,
        }
    }
    i
}

/// If the predicate is a supported seed anchor predicate, return the target variable name.
fn anchor_from_property_predicate_target_variable(predicate: &Expr) -> Option<&str> {
    match &predicate.kind {
        ExprKind::IsLabeled {
            expr,
            label: LabelExpr::Name(_),
            negated: false,
        } => variable_from_expr(expr),
        ExprKind::Compare {
            left,
            op: CmpOp::Eq,
            ..
        } => {
            if let ExprKind::PropertyAccess { expr, .. } = &left.kind {
                variable_from_expr(expr)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn parse_seed_anchor_prefix_single(
    ops: &[PlanOp],
    params: &BTreeMap<String, Value>,
    store: &RouterStore,
    stats: &RouterGraphStats,
) -> Result<Option<Vec<IndexAnchor>>, RouterError> {
    let graph_id = stats.graph_id();
    if ops.is_empty() {
        return Ok(None);
    }
    if ops.len() == 1 {
        return match &ops[0] {
            PlanOp::NodeScan { label: None, .. } => Ok(None),
            PlanOp::NodeScan {
                label: Some(label),
                variable,
                ..
            } => Ok(Some(vec![label_anchor(
                store,
                graph_id,
                label.as_ref(),
                variable,
            )?])),
            _ => match index_anchor_from_prefix_ops(ops, params, store, graph_id)? {
                Some(anchor) => Ok(Some(vec![anchor])),
                None => Ok(None),
            },
        };
    }

    let mut anchors = Vec::new();
    let mut bound_var: Option<String> = None;
    let mut multi_variable = false;
    let mut i = 0;
    while i < ops.len() {
        match &ops[i] {
            PlanOp::NodeScan {
                label: Some(label),
                variable,
                ..
            } => {
                if !record_bound_var(&mut bound_var, variable)? {
                    break;
                }
                push_unique_anchor(
                    &mut anchors,
                    label_anchor(store, graph_id, label.as_ref(), variable)?,
                );
            }
            PlanOp::NodeScan { label: None, .. } => break,
            // The prefix arm must precede the equality arm: a TextPrefix bound
            // rides `cmp = Eq` and would otherwise reach `resolve_scan_value`
            // unexpanded, which is a malformed-plan rejection.
            PlanOp::IndexScan {
                value: ScanValue::TextPrefix(pattern),
                cmp: CmpOp::Eq,
                variable,
                property,
                ..
            } => {
                if !record_bound_var(&mut bound_var, variable)? {
                    break;
                }
                if let Some(anchor) =
                    prefix_anchor(store, graph_id, params, variable, property, pattern)?
                {
                    push_unique_anchor(&mut anchors, anchor);
                }
            }
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
                ..
            } if *cmp == CmpOp::Eq => {
                if !record_bound_var(&mut bound_var, variable)? {
                    break;
                }
                // An InList bound expands into its union anchor before
                // `resolve_scan_value` can see it, mirroring the vertex
                // single-op and edge extraction paths.
                let anchor = match value {
                    ScanValue::InList(elements) => {
                        equal_union_anchor(store, graph_id, params, variable, property, elements)?
                    }
                    single => Some(equal_anchor(
                        store,
                        graph_id,
                        params,
                        variable,
                        property.as_ref(),
                        single,
                    )?),
                };
                if let Some(anchor) = anchor {
                    push_unique_anchor(&mut anchors, anchor);
                }
            }
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
                ..
            } if matches!(cmp, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) => {
                if !record_bound_var(&mut bound_var, variable)? {
                    break;
                }
                if let Some(anchor) = range_anchor(
                    store,
                    graph_id,
                    params,
                    variable.as_ref(),
                    property.as_ref(),
                    value,
                    *cmp,
                )? {
                    push_unique_anchor(&mut anchors, anchor);
                }
            }
            PlanOp::IndexIntersection {
                variable, scans, ..
            } if scans.len() >= 2 && scans.iter().all(|scan| scan.cmp == CmpOp::Eq) => {
                if !record_bound_var(&mut bound_var, variable)? {
                    break;
                }
                let mut specs = Vec::with_capacity(scans.len());
                for scan in scans {
                    let payload_bytes =
                        resolve_scan_value(&scan.value, params)?.ok_or_else(|| {
                            RouterError::InvalidArgument("missing seed parameter".into())
                        })?;
                    let property_id = store
                        .lookup_property_id(graph_id, scan.property.as_ref())
                        .map_err(|_| {
                            RouterError::NotFound(format!("property {}", scan.property.as_ref()))
                        })?
                        .raw();
                    let physical_index_id = indexed_catalog::active_vertex_physical_index(
                        graph_id,
                        None,
                        PropertyId::from_raw(property_id),
                    )?;
                    specs.push(IndexEqualSpec::vertex(
                        physical_index_id,
                        property_id,
                        payload_bytes,
                    ));
                }
                push_unique_anchor(
                    &mut anchors,
                    IndexAnchor::Intersection {
                        variable: variable.to_string(),
                        specs,
                    },
                );
            }
            PlanOp::EdgeIndexScan {
                variable,
                property,
                value,
                cmp,
                ..
            } => {
                if !record_bound_var(&mut bound_var, variable)? {
                    break;
                }
                let edge_label = ops.get(i + 1).and_then(|next| match next {
                    PlanOp::EdgeBindEndpoints {
                        label: Some(label),
                        direction,
                        ..
                    } => Some((label.as_ref(), *direction)),
                    _ => None,
                });
                // The prefix arm must precede the equality arm: a TextPrefix
                // bound rides `cmp = Eq` and would otherwise reach
                // `resolve_scan_value` unexpanded, which is a malformed-plan
                // rejection. Mirrors the vertex IndexScan ordering above.
                if let (CmpOp::Eq, ScanValue::TextPrefix(pattern)) = (cmp, value) {
                    if let Some(anchor) = edge_prefix_anchor(
                        store,
                        graph_id,
                        params,
                        variable,
                        property,
                        pattern,
                        edge_label.map(|(label, _)| label),
                        edge_label.map(|(_, direction)| direction),
                    )? {
                        push_unique_anchor(&mut anchors, anchor);
                    }
                    break;
                }
                let anchor = match cmp {
                    CmpOp::Eq => Some(match value {
                        ScanValue::InList(elements) => edge_equal_union_anchor(
                            store,
                            graph_id,
                            params,
                            variable,
                            property,
                            elements,
                            edge_label.map(|(label, _)| label),
                            edge_label.map(|(_, direction)| direction),
                        )?,
                        single => edge_equal_anchor(
                            store,
                            graph_id,
                            params,
                            variable,
                            property.as_ref(),
                            single,
                            edge_label.map(|(label, _)| label),
                            edge_label.map(|(_, direction)| direction),
                        )?,
                    }),
                    CmpOp::Ne => None,
                    _ => edge_range_anchor(
                        store,
                        graph_id,
                        params,
                        variable,
                        property.as_ref(),
                        value,
                        *cmp,
                        edge_label.map(|(label, _)| label),
                        edge_label.map(|(_, direction)| direction),
                    )?,
                };
                if let Some(anchor) = anchor {
                    push_unique_anchor(&mut anchors, anchor);
                }
                break;
            }
            PlanOp::IndexScan { .. } | PlanOp::IndexIntersection { .. } => {
                break;
            }
            PlanOp::PropertyFilter { predicates, .. } => {
                for predicate in predicates {
                    if let Some(anchor) = anchor_from_property_predicate(
                        predicate,
                        bound_var.as_deref(),
                        params,
                        store,
                        stats,
                    )? {
                        if !record_bound_var(&mut bound_var, anchor.variable())? {
                            multi_variable = true;
                            break;
                        }
                        push_unique_anchor(&mut anchors, anchor);
                    }
                }
                if multi_variable {
                    break;
                }
            }
            _ => break,
        }
        i += 1;
    }
    if anchors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(collapse_label_anchors(anchors)))
    }
}

fn collapse_label_anchors(anchors: Vec<IndexAnchor>) -> Vec<IndexAnchor> {
    let mut label_entries = Vec::new();
    let mut other = Vec::new();
    for anchor in anchors {
        match anchor {
            IndexAnchor::Label {
                vertex_label_id,
                variable,
            } => label_entries.push((variable, vertex_label_id)),
            other_anchor => other.push(other_anchor),
        }
    }
    if label_entries.len() < 2 {
        let mut out: Vec<IndexAnchor> = label_entries
            .into_iter()
            .map(|(variable, vertex_label_id)| IndexAnchor::Label {
                variable,
                vertex_label_id,
            })
            .collect();
        out.extend(other);
        return out;
    }
    label_entries.sort_by_key(|(_, id)| *id);
    label_entries.dedup_by_key(|(_, id)| *id);
    let variable = label_entries[0].0.clone();
    let vertex_label_ids: Vec<u32> = label_entries.into_iter().map(|(_, id)| id).collect();
    let mut out = vec![IndexAnchor::LabelIntersection {
        variable,
        vertex_label_ids,
    }];
    out.extend(other);
    out
}

fn record_bound_var(
    bound_var: &mut Option<String>,
    variable: impl AsRef<str>,
) -> Result<bool, RouterError> {
    if let Some(existing) = bound_var {
        if existing != variable.as_ref() {
            return Ok(false);
        }
    } else {
        *bound_var = Some(variable.as_ref().to_string());
    }
    Ok(true)
}

fn label_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    label: &str,
    variable: impl AsRef<str>,
) -> Result<IndexAnchor, RouterError> {
    Ok(IndexAnchor::Label {
        variable: variable.as_ref().to_string(),
        vertex_label_id: resolve_vertex_label_id(store, graph_id, label)?,
    })
}

fn equal_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: impl AsRef<str>,
    property: &str,
    value: &ScanValue,
) -> Result<IndexAnchor, RouterError> {
    let payload_bytes = resolve_scan_value(value, params)?
        .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
    let property_id = store
        .lookup_property_id(graph_id, property)
        .map_err(|_| RouterError::NotFound(format!("property {property}")))?
        .raw();
    let physical_index_id = indexed_catalog::active_vertex_physical_index(
        graph_id,
        None,
        PropertyId::from_raw(property_id),
    )?;
    Ok(IndexAnchor::Equal(SeedProbe {
        variable: variable.as_ref().to_string(),
        property: property.to_string(),
        property_id,
        physical_index_id,
        payload_bytes,
    }))
}

/// Build a range anchor from a leading non-Eq `IndexScan` bound.
///
/// Returns `Ok(None)` when the literal's comparison domain cannot form a domain-clamped
/// interval (Bool/List/Record/Path/Extension/Duration): such predicates cannot seed a
/// shard and must be answered by the non-index filter path instead.
fn range_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: &str,
    property: &str,
    value: &ScanValue,
    cmp: CmpOp,
) -> Result<Option<IndexAnchor>, RouterError> {
    let Some(bound) = SeedRangeBound::try_from_cmp(cmp) else {
        return Err(RouterError::InvalidArgument(format!(
            "range seed anchor requires a one-sided comparison, got {cmp:?}"
        )));
    };
    let bound_bytes = resolve_scan_value(value, params)?
        .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
    if gleaph_gql::value_index_key::range_bounds_for_encoded_key(&bound_bytes, bound.to_cmp())
        .is_err()
    {
        return Ok(None);
    }
    let property_id = store
        .lookup_property_id(graph_id, property)
        .map_err(|_| RouterError::NotFound(format!("property {property}")))?
        .raw();
    let physical_index_id = indexed_catalog::active_vertex_physical_index(
        graph_id,
        None,
        PropertyId::from_raw(property_id),
    )?;
    Ok(Some(IndexAnchor::Range(RangeSeedProbe {
        variable: variable.to_string(),
        property: property.to_string(),
        property_id,
        physical_index_id,
        bound_bytes,
        bound,
    })))
}

/// Build a text prefix anchor from a leading `IndexScan` whose bound is
/// [`ScanValue::TextPrefix`].
///
/// Returns `Ok(None)` when the resolved pattern is Null or cannot form a TEXT
/// prefix interval (non-TEXT domain, malformed encoded key): such predicates
/// seed no rows through the prefix path and must be answered by the non-index
/// filter path instead.
fn prefix_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: &str,
    property: &str,
    pattern: &ScanValue,
) -> Result<Option<IndexAnchor>, RouterError> {
    let Some(pattern_bytes) = resolve_scan_value(pattern, params)? else {
        // A Null pattern matches no row under three-valued logic; the residual
        // filter path answers the predicate without a seed.
        return Ok(None);
    };
    if gleaph_gql::value_index_key::text_prefix_range_bounds_for_encoded_key(&pattern_bytes)
        .is_err()
    {
        return Ok(None);
    }
    let property_id = store
        .lookup_property_id(graph_id, property)
        .map_err(|_| RouterError::NotFound(format!("property {property}")))?
        .raw();
    let physical_index_id = indexed_catalog::active_vertex_physical_index(
        graph_id,
        None,
        PropertyId::from_raw(property_id),
    )?;
    Ok(Some(IndexAnchor::Prefix(PrefixSeedProbe {
        variable: variable.to_string(),
        property: property.to_string(),
        property_id,
        physical_index_id,
        pattern_bytes,
    })))
}

fn anchor_from_property_predicate(
    predicate: &Expr,
    bound_var: Option<&str>,
    params: &BTreeMap<String, Value>,
    store: &RouterStore,
    stats: &RouterGraphStats,
) -> Result<Option<IndexAnchor>, RouterError> {
    match &predicate.kind {
        ExprKind::IsLabeled {
            expr,
            label: LabelExpr::Name(label),
            negated: false,
        } => {
            let Some(variable) = variable_from_expr(expr) else {
                return Ok(None);
            };
            if bound_var.is_some_and(|v| v != variable) {
                return Ok(None);
            }
            Ok(Some(label_anchor(
                store,
                stats.graph_id(),
                label,
                variable,
            )?))
        }
        ExprKind::Compare {
            left,
            op: CmpOp::Eq,
            right,
        } => {
            let Some((variable, property)) = indexed_property_access(left, stats) else {
                return Ok(None);
            };
            if bound_var.is_some_and(|v| v != variable) {
                return Ok(None);
            }
            let payload_bytes = value_to_index_key_bytes(expr_literal_or_param(right, params)?)
                .map_err(|_| {
                    RouterError::InvalidArgument("seed filter value is not indexable".into())
                })?
                .ok_or_else(|| RouterError::InvalidArgument("seed filter rejects null".into()))?;
            validate_index_value_key_bytes(&payload_bytes).map_err(|_| {
                RouterError::InvalidArgument("index value key exceeds maximum encoded size".into())
            })?;
            let property_id = store
                .lookup_property_id(stats.graph_id(), &property)
                .map_err(|_| RouterError::NotFound(format!("property {property}")))?
                .raw();
            let physical_index_id = indexed_catalog::active_vertex_physical_index(
                stats.graph_id(),
                None,
                PropertyId::from_raw(property_id),
            )?;
            Ok(Some(IndexAnchor::Equal(SeedProbe {
                variable,
                property,
                property_id,
                physical_index_id,
                payload_bytes,
            })))
        }
        _ => Ok(None),
    }
}

fn indexed_property_access(expr: &Expr, stats: &RouterGraphStats) -> Option<(String, String)> {
    match &expr.kind {
        ExprKind::PropertyAccess { expr, property } => {
            let variable = variable_from_expr(expr)?.to_string();
            if stats.is_vertex_property_indexed(property) {
                Some((variable, property.clone()))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn expr_literal_or_param<'a>(
    expr: &'a Expr,
    params: &'a BTreeMap<String, Value>,
) -> Result<&'a Value, RouterError> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(value),
        ExprKind::Parameter(name) => params
            .get(name.as_str())
            .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into())),
        _ => Err(RouterError::InvalidArgument(
            "seed filter expects literal or parameter".into(),
        )),
    }
}

fn push_unique_anchor(anchors: &mut Vec<IndexAnchor>, anchor: IndexAnchor) -> bool {
    if anchors
        .iter()
        .any(|existing| same_anchor_restriction(existing, &anchor))
    {
        return false;
    }
    anchors.push(anchor);
    true
}

fn same_anchor_restriction(left: &IndexAnchor, right: &IndexAnchor) -> bool {
    match (left, right) {
        (
            IndexAnchor::Label {
                vertex_label_id: l, ..
            },
            IndexAnchor::Label {
                vertex_label_id: r, ..
            },
        ) => l == r,
        (
            IndexAnchor::Equal(SeedProbe {
                property_id: l,
                payload_bytes: lb,
                ..
            }),
            IndexAnchor::Equal(SeedProbe {
                property_id: r,
                payload_bytes: rb,
                ..
            }),
        ) => l == r && lb == rb,
        _ => false,
    }
}

fn extract_from_ops(
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    store: &RouterStore,
    graph_id: GraphId,
) -> Result<Option<IndexAnchor>, RouterError> {
    for op in ops {
        if let Some(anchor) = extract_from_op(op, parameters, store, graph_id)? {
            return Ok(Some(anchor));
        }
    }
    Ok(None)
}

fn extract_from_op(
    op: &PlanOp,
    parameters: &BTreeMap<String, Value>,
    store: &RouterStore,
    graph_id: GraphId,
) -> Result<Option<IndexAnchor>, RouterError> {
    match op {
        PlanOp::IndexIntersection {
            variable, scans, ..
        } if scans.len() >= 2 => {
            let mut specs = Vec::with_capacity(scans.len());
            for scan in scans {
                if scan.cmp != CmpOp::Eq {
                    return Ok(None);
                }
                let payload_bytes = resolve_scan_value(&scan.value, parameters)?
                    .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
                let property_id = store
                    .lookup_property_id(graph_id, scan.property.as_ref())
                    .map_err(|_| {
                        RouterError::NotFound(format!("property {}", scan.property.as_ref()))
                    })?
                    .raw();
                let physical_index_id = indexed_catalog::active_vertex_physical_index(
                    graph_id,
                    None,
                    PropertyId::from_raw(property_id),
                )?;
                specs.push(IndexEqualSpec::vertex(
                    physical_index_id,
                    property_id,
                    payload_bytes,
                ));
            }
            Ok(Some(IndexAnchor::Intersection {
                variable: variable.to_string(),
                specs,
            }))
        }
        PlanOp::NodeScan {
            variable,
            label: Some(label),
            ..
        } => Ok(Some(IndexAnchor::Label {
            variable: variable.to_string(),
            vertex_label_id: resolve_vertex_label_id(store, graph_id, label.as_ref())?,
        })),
        PlanOp::IndexScan {
            variable,
            property,
            value: ScanValue::TextPrefix(pattern),
            cmp: CmpOp::Eq,
            ..
        } => prefix_anchor(store, graph_id, parameters, variable, property, pattern),
        PlanOp::IndexScan {
            variable,
            property,
            value: ScanValue::InList(elements),
            cmp: CmpOp::Eq,
            ..
        } => equal_union_anchor(store, graph_id, parameters, variable, property, elements),
        PlanOp::IndexScan {
            variable,
            property,
            value,
            cmp,
            ..
        } if *cmp == CmpOp::Eq => {
            let payload_bytes = resolve_scan_value(value, parameters)?
                .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
            let property_id = store
                .lookup_property_id(graph_id, property.as_ref())
                .map_err(|_| RouterError::NotFound(format!("property {}", property.as_ref())))?
                .raw();
            let physical_index_id = indexed_catalog::active_vertex_physical_index(
                graph_id,
                None,
                PropertyId::from_raw(property_id),
            )?;
            Ok(Some(IndexAnchor::Equal(SeedProbe {
                variable: variable.to_string(),
                property: property.to_string(),
                property_id,
                physical_index_id,
                payload_bytes,
            })))
        }
        PlanOp::IndexScan {
            variable,
            property,
            value,
            cmp,
            ..
        } if matches!(cmp, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) => range_anchor(
            store,
            graph_id,
            parameters,
            variable.as_ref(),
            property.as_ref(),
            value,
            *cmp,
        ),
        PlanOp::EdgeIndexScan {
            variable,
            property,
            value: ScanValue::TextPrefix(pattern),
            cmp: CmpOp::Eq,
            ..
        } => edge_prefix_anchor(
            store, graph_id, parameters, variable, property, pattern, None, None,
        ),
        PlanOp::EdgeIndexScan {
            variable,
            property,
            value,
            cmp,
            ..
        } if *cmp == CmpOp::Eq => Ok(Some(match value {
            ScanValue::InList(elements) => edge_equal_union_anchor(
                store, graph_id, parameters, variable, property, elements, None, None,
            )?,
            single => edge_equal_anchor(
                store,
                graph_id,
                parameters,
                variable,
                property.as_ref(),
                single,
                None,
                None,
            )?,
        })),
        PlanOp::EdgeIndexScan {
            variable,
            property,
            value,
            cmp,
            ..
        } if matches!(cmp, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) => edge_range_anchor(
            store,
            graph_id,
            parameters,
            variable,
            property.as_ref(),
            value,
            *cmp,
            None,
            None,
        ),
        PlanOp::HashJoin { left, right, .. } => {
            if let Some(p) = extract_from_ops(left, parameters, store, graph_id)? {
                return Ok(Some(p));
            }
            extract_from_ops(right, parameters, store, graph_id)
        }
        PlanOp::CartesianProduct { left, right } => {
            if let Some(p) = extract_from_ops(left, parameters, store, graph_id)? {
                return Ok(Some(p));
            }
            extract_from_ops(right, parameters, store, graph_id)
        }
        PlanOp::OptionalMatch { sub_plan } => {
            extract_from_ops(sub_plan, parameters, store, graph_id)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            extract_from_ops(&sub_plan.ops, parameters, store, graph_id)
        }
        PlanOp::UseGraph {
            sub_plan: Some(sp), ..
        } => extract_from_ops(sp, parameters, store, graph_id),
        PlanOp::SetOperation { right, .. } => {
            extract_from_ops(&right.ops, parameters, store, graph_id)
        }
        _ => Ok(None),
    }
}

pub(crate) fn resolve_scan_value(
    value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<Vec<u8>>, RouterError> {
    let bytes = match value {
        ScanValue::Literal(v) => value_to_index_key_bytes(v).map_err(|_| {
            RouterError::InvalidArgument("seed filter value is not indexable".into())
        })?,
        ScanValue::Parameter(name) => {
            let v = parameters
                .get(name.as_ref())
                .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
            value_to_index_key_bytes(v).map_err(|_| {
                RouterError::InvalidArgument("seed filter value is not indexable".into())
            })?
        }
        // Nested IN lists are not produced by the planner: an InList IndexScan is
        // expanded into per-element probes by `equal_union_anchor` before seed
        // resolution, so reaching this arm means a malformed plan.
        ScanValue::InList(_) => {
            return Err(RouterError::InvalidArgument(
                "IN-list scan values must be expanded into per-element probes".into(),
            ));
        }
        // A TextPrefix IndexScan is lowered into a Between prefix seed probe by
        // `prefix_anchor` before seed resolution; reaching this arm means a
        // malformed plan.
        ScanValue::TextPrefix(_) => {
            return Err(RouterError::InvalidArgument(
                "text-prefix scan values must be expanded into a Between range seed probe".into(),
            ));
        }
    };
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    validate_index_value_key_bytes(&bytes).map_err(|_| {
        RouterError::InvalidArgument("index value key exceeds maximum encoded size".into())
    })?;
    Ok(Some(bytes))
}

/// Build the union-of-point-probes anchor for a leading equality `IndexScan`
/// whose bound is [`ScanValue::InList`].
///
/// Each element resolves independently; Null elements contribute no probe
/// because no posting can equal them, while unencodable or missing-parameter
/// elements fail closed through [`resolve_scan_value`]. An empty probe list is
/// kept: the union of nothing matches zero rows, which mirrors `v.prop IN []`
/// semantics.
fn equal_union_anchor(
    store: &RouterStore,
    graph_id: GraphId,
    params: &BTreeMap<String, Value>,
    variable: &Str,
    property: &Str,
    elements: &[ScanValue],
) -> Result<Option<IndexAnchor>, RouterError> {
    let property_id = store
        .lookup_property_id(graph_id, property.as_ref())
        .map_err(|_| RouterError::NotFound(format!("property {}", property.as_ref())))?
        .raw();
    let physical_index_id = indexed_catalog::active_vertex_physical_index(
        graph_id,
        None,
        PropertyId::from_raw(property_id),
    )?;
    let mut probes = Vec::with_capacity(elements.len());
    for element in elements {
        let Some(payload_bytes) = resolve_scan_value(element, params)? else {
            continue;
        };
        probes.push(SeedProbe {
            variable: variable.to_string(),
            property: property.to_string(),
            property_id,
            physical_index_id,
            payload_bytes,
        });
    }
    Ok(Some(IndexAnchor::EqualUnion {
        variable: variable.to_string(),
        probes,
    }))
}

/// Encode local vertex ids for one shard into `ExecutePlanArgs.seed_bindings_blob`.
///
/// Filters `hits` to `target_shard` only; returns `None` when that shard has no hits.
pub fn seeds_for_local_shard(
    variable: &str,
    hits: &[PostingHit],
    target_shard: ShardId,
) -> Option<Vec<u8>> {
    let local_ids: Vec<u32> = hits
        .iter()
        .filter(|h| h.shard_id == target_shard)
        .map(|h| h.vertex_id)
        .collect();
    if local_ids.is_empty() {
        return None;
    }
    let wire = SeedBindingsWire {
        entries: vec![SeedBindingEntry {
            variable: variable.to_string(),
            local_vertex_ids: local_ids,
            local_edge_postings: Vec::new(),
        }],
        rows: Vec::new(),
        complete_prefix_rows: false,
    };
    Some(Encode!(&wire).expect("SeedBindingsWire encode"))
}

/// Encode local edge postings for one shard into `ExecutePlanArgs.seed_bindings_blob`.
pub fn seeds_for_local_shard_edges(
    variable: &str,
    hits: &[EdgePostingHit],
    target_shard: ShardId,
) -> Option<Vec<u8>> {
    let local_postings: Vec<LocalEdgePosting> = hits
        .iter()
        .filter(|h| h.shard_id == target_shard)
        .map(|h| LocalEdgePosting {
            owner_vertex_id: h.owner_vertex_id,
            label_id: h.label_id,
            slot_index: h.slot_index,
        })
        .collect();
    if local_postings.is_empty() {
        return None;
    }
    let wire = SeedBindingsWire {
        entries: vec![SeedBindingEntry {
            variable: variable.to_string(),
            local_vertex_ids: Vec::new(),
            local_edge_postings: local_postings,
        }],
        rows: Vec::new(),
        complete_prefix_rows: false,
    };
    Some(Encode!(&wire).expect("SeedBindingsWire encode"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use candid::{Decode, Encode};
    use gleaph_gql::Value;
    use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
    use gleaph_gql::types::LabelExpr;
    use gleaph_gql_planner::NodeLabelRef;
    use gleaph_gql_planner::PhysicalPlan;
    use gleaph_gql_planner::plan::{IndexScanSpec, PlanOp, ScanValue};
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::PostingHit;
    use gleaph_graph_kernel::plan_exec::SeedBindingsWire;

    use gleaph_graph_kernel::index::EdgePostingHit;

    use super::{
        IndexAnchor, SeedAnchorSet, SeedProbe, SeedRangeBound, seeds_for_local_shard,
        seeds_for_local_shard_edges,
    };
    use crate::facade::store::RouterStore;
    use crate::planner_stats::RouterGraphStats;

    fn test_store_with_property(property: &str) -> (RouterStore, GraphId) {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            property,
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, property,
        );
        (store, graph_id)
    }

    fn index_scan_plan(property: &str, value: Value) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![PlanOp::IndexScan {
            variable: Rc::from("u"),
            property: Rc::from(property),
            value: ScanValue::Literal(value),
            cmp: CmpOp::Eq,
            property_projection: None,
        }])
    }

    #[test]
    fn index_anchor_from_plans_finds_index_intersection() {
        let (store, graph_id) = test_store_with_property("uid");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            "tenant.main",
            "email",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, "email",
        );
        let plan = PhysicalPlan::from_ops(vec![PlanOp::IndexIntersection {
            variable: Rc::from("n"),
            scans: vec![
                IndexScanSpec {
                    property: Rc::from("uid"),
                    value: ScanValue::Literal(Value::Text("alice".into())),
                    cmp: CmpOp::Eq,
                },
                IndexScanSpec {
                    property: Rc::from("email"),
                    value: ScanValue::Literal(Value::Text("alice@example.com".into())),
                    cmp: CmpOp::Eq,
                },
            ],
            property_projection: None,
        }]);
        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor")
        .expect("intersection anchor");
        assert_eq!(anchor.variable(), "n");
        let IndexAnchor::Intersection { specs, .. } = anchor else {
            panic!("expected intersection anchor");
        };
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].property_id, 1);
        assert_eq!(specs[1].property_id, 2);
        assert!(!specs[0].value.is_empty());
        assert!(!specs[1].value.is_empty());
        assert!(
            SeedProbe::from_plans(
                std::slice::from_ref(&plan),
                &BTreeMap::new(),
                &store,
                graph_id
            )
            .expect("probe")
            .is_none()
        );
    }

    #[test]
    fn seed_probe_from_plans_finds_equality_index_scan() {
        let (store, graph_id) = test_store_with_property("uid");
        let plan = index_scan_plan("uid", Value::Text("alice".into()));
        let mut params = BTreeMap::new();
        let probe = SeedProbe::from_plans(std::slice::from_ref(&plan), &params, &store, graph_id)
            .expect("probe")
            .expect("some probe");
        assert_eq!(probe.variable, "u");
        assert_eq!(probe.property, "uid");
        assert_eq!(probe.property_id, 1);
        assert!(!probe.payload_bytes.is_empty());

        params.insert("$x".into(), Value::Text("alice".into()));
        let plan = PhysicalPlan::from_ops(vec![PlanOp::IndexScan {
            variable: Rc::from("u"),
            property: Rc::from("uid"),
            value: ScanValue::Parameter(Rc::from("$x")),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);
        let probe = SeedProbe::from_plans(std::slice::from_ref(&plan), &params, &store, graph_id)
            .expect("probe")
            .expect("parameter probe");
        assert!(!probe.payload_bytes.is_empty());
    }

    #[test]
    fn index_anchor_from_plans_finds_multi_label_intersection() {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Person",
            )
            .expect("intern Person");
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Employee",
            )
            .expect("intern Employee");
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some(NodeLabelRef::from("Person")),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![gleaph_gql::ast::Expr::new(ExprKind::IsLabeled {
                    expr: Box::new(gleaph_gql::ast::Expr::var("n")),
                    label: LabelExpr::Name("Employee".into()),
                    negated: false,
                })],
                stage: 0,
            },
        ]);
        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor")
        .expect("label intersection");
        let IndexAnchor::LabelIntersection {
            vertex_label_ids, ..
        } = anchor
        else {
            panic!("expected label intersection anchor");
        };
        assert_eq!(vertex_label_ids, vec![1, 2]);
    }

    #[test]
    fn seed_anchor_set_finds_label_and_index_scan_prefix() {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Person",
            )
            .expect("intern Person");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "region",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, "region",
        );
        let stats = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["region"]);
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some(NodeLabelRef::from("Person")),
                property_projection: None,
            },
            PlanOp::IndexScan {
                variable: Rc::from("n"),
                property: Rc::from("region"),
                value: ScanValue::Literal(Value::Text("US".into())),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);
        let set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("anchors")
        .expect("compound anchors");
        assert_eq!(set.variable(), "n");
        assert_eq!(set.anchors().len(), 2);
        assert!(set.anchors().iter().any(|anchor| {
            matches!(
                anchor,
                IndexAnchor::Label {
                    vertex_label_id: 1,
                    ..
                }
            )
        }));
        assert!(
            set.anchors()
                .iter()
                .any(|anchor| matches!(anchor, IndexAnchor::Equal(_)))
        );
    }

    #[test]
    fn inlist_index_scan_extracts_equal_union_anchor_with_per_element_probes() {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Person",
            )
            .expect("intern Person");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "region",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, "region",
        );
        let stats = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["region"]);
        let plan = PhysicalPlan::from_ops(vec![PlanOp::IndexScan {
            variable: Rc::from("n"),
            property: Rc::from("region"),
            value: ScanValue::InList(vec![
                ScanValue::Literal(Value::Text("US".into())),
                // A Null can never satisfy equality: no probe for it.
                ScanValue::Literal(Value::Null),
                ScanValue::Parameter(Rc::from("$rest")),
            ]),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);
        let mut params = BTreeMap::new();
        params.insert("$rest".to_string(), Value::Text("EU".into()));

        let set = SeedAnchorSet::from_plans(std::slice::from_ref(&plan), &params, &store, &stats)
            .expect("anchors")
            .expect("seed anchors");

        let Some(IndexAnchor::EqualUnion { variable, probes }) = set
            .anchors()
            .iter()
            .find(|anchor| matches!(anchor, IndexAnchor::EqualUnion { .. }))
        else {
            panic!("expected EqualUnion anchor, got: {:?}", set.anchors());
        };
        assert_eq!(variable, "n");
        assert_eq!(probes.len(), 2, "Null contributes no probe");
        let us = gleaph_gql::value_to_index_key_bytes(&Value::Text("US".into()))
            .expect("encode US")
            .expect("US bytes");
        let eu = gleaph_gql::value_to_index_key_bytes(&Value::Text("EU".into()))
            .expect("encode EU")
            .expect("EU bytes");
        assert!(probes.iter().any(|probe| probe.payload_bytes == us));
        assert!(probes.iter().any(|probe| probe.payload_bytes == eu));
        assert!(probes.iter().all(|probe| probe.property == "region"));
    }

    /// Catalog fixture registering an active vertex index over `name`.
    fn prefix_seed_setup() -> (RouterStore, GraphId) {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "name",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, "name",
        );
        (store, graph_id)
    }

    #[test]
    fn prefix_index_scan_extracts_prefix_anchor_single_op() {
        let (store, graph_id) = prefix_seed_setup();
        let stats = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["name"]);
        let plan = PhysicalPlan::from_ops(vec![PlanOp::IndexScan {
            variable: Rc::from("n"),
            property: Rc::from("name"),
            value: ScanValue::TextPrefix(Box::new(ScanValue::Literal(Value::Text("Str".into())))),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("anchors")
        .expect("seed anchors");
        let Some(IndexAnchor::Prefix(probe)) = set
            .anchors()
            .iter()
            .find(|anchor| matches!(anchor, IndexAnchor::Prefix(_)))
        else {
            panic!("expected Prefix anchor, got: {:?}", set.anchors());
        };
        assert_eq!(probe.variable, "n");
        assert_eq!(probe.property, "name");
        // The probe carries the fully encoded pattern key including terminator.
        assert_eq!(
            probe.pattern_bytes,
            gleaph_gql::value_to_index_key_bytes(&Value::Text("Str".into()))
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    fn prefix_index_scan_extracts_prefix_anchor_with_trailing_residual_ops() {
        // The planner keeps the STARTS WITH predicate in a residual PropertyFilter
        // after the leading scan; seed extraction must still find the anchor.
        let (store, graph_id) = prefix_seed_setup();
        let stats = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["name"]);
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: Rc::from("n"),
                property: Rc::from("name"),
                value: ScanValue::TextPrefix(Box::new(ScanValue::Parameter(Rc::from("$pre")))),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::var("n")],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);
        let mut params = BTreeMap::new();
        params.insert("$pre".to_string(), Value::Text("StrPred".into()));

        let set = SeedAnchorSet::from_plans(std::slice::from_ref(&plan), &params, &store, &stats)
            .expect("anchors")
            .expect("seed anchors");
        assert!(
            set.anchors().iter().any(
                |anchor| matches!(anchor, IndexAnchor::Prefix(probe) if probe.variable == "n")
            ),
            "expected Prefix anchor behind residual ops, got: {:?}",
            set.anchors()
        );
    }

    #[test]
    fn prefix_index_scan_null_pattern_extracts_no_anchor() {
        let (store, graph_id) = prefix_seed_setup();
        let stats = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["name"]);
        let plan = PhysicalPlan::from_ops(vec![PlanOp::IndexScan {
            variable: Rc::from("n"),
            property: Rc::from("name"),
            value: ScanValue::TextPrefix(Box::new(ScanValue::Literal(Value::Null))),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("anchors");
        assert!(
            set.is_none(),
            "a Null pattern seeds no rows and must not produce an anchor"
        );
    }

    #[test]
    fn resolve_scan_value_rejects_raw_text_prefix() {
        let err = super::resolve_scan_value(
            &ScanValue::TextPrefix(Box::new(ScanValue::Literal(Value::Text("x".into())))),
            &BTreeMap::new(),
        )
        .expect_err("raw text-prefix bound must be rejected");
        assert!(err.to_string().contains("text-prefix"));
    }

    #[test]
    fn inlist_index_scan_with_trailing_ops_extracts_equal_union() {
        // Regression: the multi-op equality arm used to pass InList bounds straight
        // to `equal_anchor`, whose raw-bound rejection failed the whole extraction
        // even though the single-op path expanded them into EqualUnion probes.
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Person",
            )
            .expect("intern Person");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "region",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, "region",
        );
        let stats = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["region"]);
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: Rc::from("n"),
                property: Rc::from("region"),
                value: ScanValue::InList(vec![ScanValue::Literal(Value::Text("US".into()))]),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::var("n")],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);

        let set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("extraction must not fail on trailing residual ops")
        .expect("seed anchors");
        assert!(
            set.anchors()
                .iter()
                .any(|anchor| matches!(anchor, IndexAnchor::EqualUnion { .. })),
            "InList with trailing ops must extract EqualUnion, got: {:?}",
            set.anchors()
        );
    }

    #[test]
    fn seed_anchor_set_extracts_multi_variable_anchors_from_cartesian_product() {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Post",
            )
            .expect("intern Post");
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Topic",
            )
            .expect("intern Topic");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "demo_id",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, "demo_id",
        );
        let stats = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["demo_id"]);
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::CartesianProduct {
                left: vec![PlanOp::IndexScan {
                    variable: Rc::from("a"),
                    property: Rc::from("demo_id"),
                    value: ScanValue::Literal(Value::Text("post-demo".into())),
                    cmp: CmpOp::Eq,
                    property_projection: None,
                }],
                right: vec![PlanOp::IndexScan {
                    variable: Rc::from("b"),
                    property: Rc::from("demo_id"),
                    value: ScanValue::Literal(Value::Text("topic-gql".into())),
                    cmp: CmpOp::Eq,
                    property_projection: None,
                }],
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);
        let set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("anchors")
        .expect("cartesian anchors");
        let vars: Vec<&str> = set.variables.iter().map(|v| v.variable.as_str()).collect();
        assert_eq!(vars, vec!["a", "b"]);
    }

    #[test]
    fn index_anchor_from_plans_finds_labeled_node_scan() {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        store
            .admin_intern_vertex_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "Person",
            )
            .expect("intern Person");
        let plan = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
            variable: Rc::from("n"),
            label: Some(NodeLabelRef::from("Person")),
            property_projection: None,
        }]);
        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor")
        .expect("label anchor");
        assert_eq!(anchor.variable(), "n");
        let IndexAnchor::Label {
            vertex_label_id, ..
        } = anchor
        else {
            panic!("expected label anchor");
        };
        assert_eq!(vertex_label_id, 1);
    }

    #[test]
    fn seeds_for_local_shard_encodes_matching_vertices_only() {
        let hits = vec![
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 10,
            },
            PostingHit {
                shard_id: ShardId::new(1),
                vertex_id: 20,
            },
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 11,
            },
        ];
        let blob = seeds_for_local_shard("u", &hits, ShardId::new(0)).expect("seed blob");
        let wire: SeedBindingsWire = Decode!(&blob, SeedBindingsWire).expect("decode");
        assert_eq!(wire.entries.len(), 1);
        assert_eq!(wire.entries[0].variable, "u");
        assert_eq!(wire.entries[0].local_vertex_ids, vec![10, 11]);

        let roundtrip: SeedBindingsWire =
            Decode!(&Encode!(&wire).expect("re-encode"), SeedBindingsWire).expect("roundtrip");
        assert_eq!(wire.entries, roundtrip.entries);
        assert_eq!(wire.rows, roundtrip.rows);
        assert!(seeds_for_local_shard("u", &hits, ShardId::new(99)).is_none());
    }

    #[test]
    fn edge_index_scan_anchor_resolves_label_and_encodes_edge_seeds() {
        use gleaph_gql::types::EdgeDirection;
        use gleaph_gql_planner::plan::EdgeLabelRef;

        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "weight",
        );
        store
            .admin_intern_edge_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "KNOWS",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::register_active_edge_index(
            &store, graph_id, "KNOWS", "weight",
        );
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::EdgeIndexScan {
                variable: Rc::from("e"),
                property: Rc::from("weight"),
                value: ScanValue::Literal(Value::Int64(5)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: Rc::from("e"),
                near: Rc::from("a"),
                far: Rc::from("b"),
                direction: EdgeDirection::PointingRight,
                label: Some(EdgeLabelRef::from("KNOWS")),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
        ]);
        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor")
        .expect("edge anchor");
        let IndexAnchor::EdgeEqual(probe) = anchor else {
            panic!("expected EdgeEqual anchor");
        };
        assert_eq!(probe.variable, "e");
        assert_eq!(probe.wire_label_ids, vec![0x8001]);

        let hits = vec![
            EdgePostingHit {
                shard_id: ShardId::new(0),
                owner_vertex_id: 3,
                label_id: 0x8001,
                slot_index: 2,
            },
            EdgePostingHit {
                shard_id: ShardId::new(1),
                owner_vertex_id: 9,
                label_id: 0x8001,
                slot_index: 0,
            },
        ];
        let blob = seeds_for_local_shard_edges("e", &hits, ShardId::new(0)).expect("edge seeds");
        let wire: SeedBindingsWire = Decode!(&blob, SeedBindingsWire).expect("decode");
        assert_eq!(wire.entries[0].local_edge_postings.len(), 1);
        assert_eq!(wire.entries[0].local_edge_postings[0].owner_vertex_id, 3);
        assert_eq!(wire.entries[0].local_edge_postings[0].slot_index, 2);
    }

    #[test]
    fn edge_inlist_index_scan_extracts_edge_equal_union_with_per_element_probes() {
        use gleaph_gql::types::EdgeDirection;
        use gleaph_gql_planner::plan::EdgeLabelRef;

        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "weight",
        );
        store
            .admin_intern_edge_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "KNOWS",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::register_active_edge_index(
            &store, graph_id, "KNOWS", "weight",
        );
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::EdgeIndexScan {
                variable: Rc::from("e"),
                property: Rc::from("weight"),
                value: ScanValue::InList(vec![
                    ScanValue::Literal(Value::Int64(5)),
                    // A Null can never satisfy equality: no probe for it.
                    ScanValue::Literal(Value::Null),
                    ScanValue::Parameter(Rc::from("$rest")),
                ]),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: Rc::from("e"),
                near: Rc::from("a"),
                far: Rc::from("b"),
                direction: EdgeDirection::PointingRight,
                label: Some(EdgeLabelRef::from("KNOWS")),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
        ]);
        let mut params = BTreeMap::new();
        params.insert("$rest".to_string(), Value::Int64(9));

        let anchor =
            IndexAnchor::from_plans(std::slice::from_ref(&plan), &params, &store, graph_id)
                .expect("anchor")
                .expect("edge equal union anchor");
        let IndexAnchor::EdgeEqualUnion { variable, probes } = anchor else {
            panic!("expected EdgeEqualUnion anchor, got {anchor:?}");
        };
        assert_eq!(variable, "e");
        assert_eq!(probes.len(), 2, "Null contributes no probe");
        let five = gleaph_gql::value_to_index_key_bytes(&Value::Int64(5))
            .expect("encode 5")
            .expect("5 bytes");
        let nine = gleaph_gql::value_to_index_key_bytes(&Value::Int64(9))
            .expect("encode 9")
            .expect("9 bytes");
        assert!(probes.iter().any(|probe| probe.payload_bytes == five));
        assert!(probes.iter().any(|probe| probe.payload_bytes == nine));
        assert!(probes.iter().all(|probe| probe.property == "weight"));
        // Every element keeps the ADR 0012 wire-label subset of the query label.
        assert!(
            probes
                .iter()
                .all(|probe| probe.wire_label_ids == vec![0x8001])
        );
    }

    #[test]
    fn resolve_scan_value_rejects_oversized_index_key() {
        use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

        let oversized = Value::Bytes(vec![1u8; MAX_INDEX_VALUE_KEY_BYTES - 2]);
        let err = super::resolve_scan_value(&ScanValue::Literal(oversized), &BTreeMap::new())
            .expect_err("oversized key");
        assert!(
            err.to_string()
                .contains("index value key exceeds maximum encoded size")
        );
    }

    #[test]
    fn edge_range_scan_anchor_lowers_one_sided_bound_with_wire_labels() {
        use gleaph_gql::types::EdgeDirection;
        use gleaph_gql_planner::plan::EdgeLabelRef;

        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "weight",
        );
        store
            .admin_intern_edge_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "KNOWS",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::register_active_edge_index(
            &store, graph_id, "KNOWS", "weight",
        );
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::EdgeIndexScan {
                variable: Rc::from("e"),
                property: Rc::from("weight"),
                value: ScanValue::Literal(Value::Int64(3)),
                cmp: CmpOp::Gt,
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: Rc::from("e"),
                near: Rc::from("a"),
                far: Rc::from("b"),
                direction: EdgeDirection::PointingRight,
                label: Some(EdgeLabelRef::from("KNOWS")),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
        ]);
        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor")
        .expect("edge range anchor");
        let IndexAnchor::EdgeRange(probe) = anchor else {
            panic!("expected EdgeRange anchor");
        };
        assert_eq!(probe.variable, "e");
        assert_eq!(probe.property_id, 1);
        assert_eq!(probe.bound, SeedRangeBound::Gt);
        assert_eq!(
            probe.bound_bytes,
            gleaph_gql::value_to_index_key_bytes(&Value::Int64(3))
                .unwrap()
                .unwrap()
        );
        // The bound must realize a domain-clamped interval; otherwise the anchor is invalid.
        assert!(
            gleaph_gql::value_index_key::range_bounds_for_encoded_key(
                &probe.bound_bytes,
                probe.bound.to_cmp(),
            )
            .is_ok()
        );
        assert_eq!(probe.wire_label_ids, vec![0x8001]);
    }

    /// Edge twin of the vertex prefix anchor: a leading `EdgeIndexScan{TextPrefix}`
    /// extracts an `EdgePrefix` probe carrying the stored pattern key and the ADR 0012
    /// wire-label subset. Null or non-TEXT resolved patterns seed no anchor.
    #[test]
    fn edge_prefix_scan_extracts_edge_prefix_anchor_with_wire_labels() {
        use gleaph_gql::types::EdgeDirection;
        use gleaph_gql_planner::plan::EdgeLabelRef;

        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "name",
        );
        store
            .admin_intern_edge_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "KNOWS",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::register_active_edge_index(
            &store, graph_id, "KNOWS", "name",
        );
        let prefix_plan = |pattern: ScanValue| {
            PhysicalPlan::from_ops(vec![
                PlanOp::EdgeIndexScan {
                    variable: Rc::from("e"),
                    property: Rc::from("name"),
                    value: pattern,
                    cmp: CmpOp::Eq,
                    property_projection: None,
                },
                PlanOp::EdgeBindEndpoints {
                    edge: Rc::from("e"),
                    near: Rc::from("a"),
                    far: Rc::from("b"),
                    direction: EdgeDirection::PointingRight,
                    label: Some(EdgeLabelRef::from("KNOWS")),
                    near_property_projection: None,
                    far_property_projection: None,
                    hop_aux_binding: None,
                },
            ])
        };

        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&prefix_plan(ScanValue::TextPrefix(Box::new(
                ScanValue::Literal(Value::Text("Str".into())),
            )))),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor")
        .expect("edge prefix anchor");
        let IndexAnchor::EdgePrefix(probe) = anchor else {
            panic!("expected EdgePrefix anchor");
        };
        assert_eq!(probe.variable, "e");
        assert_eq!(probe.property_id, 1);
        assert_eq!(
            probe.pattern_bytes,
            gleaph_gql::value_to_index_key_bytes(&Value::Text("Str".into()))
                .unwrap()
                .unwrap(),
            "the probe carries the stored TEXT key including its terminator"
        );
        // The interval derived at collection time must realize the encoded prefix bounds.
        let (low, high) = gleaph_gql::value_index_key::text_prefix_range_bounds_for_encoded_key(
            &probe.pattern_bytes,
        )
        .expect("encoded prefix interval");
        assert_eq!(low, vec![6, b'S', b't', b'r']);
        assert_eq!(high, vec![6, b'S', b't', b'r', 0xFF]);
        assert_eq!(probe.wire_label_ids, vec![0x8001]);

        // A Null pattern matches nothing under three-valued logic: no seed.
        let null_anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&prefix_plan(ScanValue::TextPrefix(Box::new(
                ScanValue::Literal(Value::Null),
            )))),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor");
        assert!(
            null_anchor.is_none(),
            "Null pattern must not produce an EdgePrefix seed"
        );
    }

    /// Interns the nested leaf path plus its top-level ancestor (the real DDL order,
    /// ADR 0073) and registers an immediate-Active vertex index on the leaf.
    fn test_store_with_nested_leaf() -> (RouterStore, GraphId) {
        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "stats",
        );
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "stats.score",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store,
            graph_id,
            0,
            "stats.score",
        );
        (store, graph_id)
    }

    #[test]
    fn vertex_equality_anchor_resolves_nested_leaf_property() {
        let (store, graph_id) = test_store_with_nested_leaf();
        let plan = index_scan_plan("stats.score", Value::Int64(3));
        let probe = SeedProbe::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("probe")
        .expect("nested leaf equality probe");
        assert_eq!(probe.variable, "u");
        assert_eq!(probe.property, "stats.score");
        assert!(!probe.payload_bytes.is_empty());
    }

    #[test]
    fn vertex_range_anchor_resolves_nested_leaf_property() {
        let (store, graph_id) = test_store_with_nested_leaf();
        let plan = PhysicalPlan::from_ops(vec![PlanOp::IndexScan {
            variable: Rc::from("u"),
            property: Rc::from("stats.score"),
            value: ScanValue::Literal(Value::Int64(18)),
            cmp: CmpOp::Ge,
            property_projection: None,
        }]);
        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor")
        .expect("nested leaf range anchor");
        let IndexAnchor::Range(probe) = anchor else {
            panic!("expected Range anchor, got {anchor:?}");
        };
        assert_eq!(probe.variable, "u");
        assert_eq!(probe.property, "stats.score");
        assert_eq!(probe.bound, SeedRangeBound::Ge);
        assert_eq!(
            probe.bound_bytes,
            gleaph_gql::value_to_index_key_bytes(&Value::Int64(18))
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    fn edge_range_scan_anchor_skips_unsupported_comparison_domain() {
        use gleaph_gql::types::EdgeDirection;
        use gleaph_gql_planner::plan::EdgeLabelRef;

        let (store, admin, graph_id) = crate::facade::store::catalog_test_support::setup();
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            crate::facade::store::catalog_test_support::GRAPH,
            "flag",
        );
        store
            .admin_intern_edge_label(
                admin,
                crate::facade::store::catalog_test_support::GRAPH,
                "KNOWS",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::register_active_edge_index(
            &store, graph_id, "KNOWS", "flag",
        );
        // Bool cannot form a domain-clamped interval: no seed anchor may be produced so the
        // query falls back to non-seeded execution and the residual filter decides.
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::EdgeIndexScan {
                variable: Rc::from("e"),
                property: Rc::from("flag"),
                value: ScanValue::Literal(Value::Bool(true)),
                cmp: CmpOp::Gt,
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: Rc::from("e"),
                near: Rc::from("a"),
                far: Rc::from("b"),
                direction: EdgeDirection::PointingRight,
                label: Some(EdgeLabelRef::from("KNOWS")),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
        ]);
        let anchor = IndexAnchor::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            graph_id,
        )
        .expect("anchor resolution succeeds");
        assert!(anchor.is_none(), "unsupported domain must not seed a shard");
    }
}
