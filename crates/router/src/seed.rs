//! Index anchor detection and per-shard seed binding for graph dispatch.
//!
//! The router resolves query entry points via graph-index before calling graph shards.
//! [`IndexAnchor`] captures the leading anchor op (`IndexScan`, `IndexIntersection`, or
//! labeled `NodeScan`); hits are encoded into `seed_bindings_blob` so shards can skip
//! that op.

use std::collections::BTreeMap;

use candid::Encode;
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
use gleaph_gql::types::LabelExpr;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_gql_planner::GraphStats;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::plan::{PlanOp, ScanValue};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{IndexEqualSpec, PostingHit};
use gleaph_graph_kernel::plan_exec::{SeedBindingEntry, SeedBindingsWire};

use crate::facade::store::RouterStore;
use crate::planner_stats::RouterGraphStats;
use crate::state::RouterError;

/// Index lookup anchor extracted from a physical plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexAnchor {
    /// Single equality `IndexScan` (`lookup_equal`).
    Equal(SeedProbe),
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
            Self::Intersection { variable, .. } => variable.as_str(),
            Self::Label { variable, .. } => variable.as_str(),
            Self::LabelIntersection { variable, .. } => variable.as_str(),
        }
    }

    /// Scan physical plans for the first index anchor (`IndexIntersection`, equality `IndexScan`, or labeled `NodeScan`).
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
    ) -> Result<Option<Self>, RouterError> {
        Ok(
            SeedAnchorSet::from_plans(plans, parameters, store, &RouterGraphStats::default())?
                .map(|set| set.routing_anchor()),
        )
    }
}

/// One or more index/label anchors on the same variable for router seed routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedAnchorSet {
    pub variable: String,
    pub anchors: Vec<IndexAnchor>,
}

impl SeedAnchorSet {
    /// Leading anchor prefix for seed routing (label + property intersection when present).
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
        stats: &RouterGraphStats,
    ) -> Result<Option<Self>, RouterError> {
        for plan in plans {
            if let Some(anchors) = parse_seed_anchor_prefix(&plan.ops, parameters, store, stats)? {
                return Ok(Some(Self {
                    variable: anchors[0].variable().to_string(),
                    anchors,
                }));
            }
        }
        Ok(None)
    }

    /// Representative anchor for [`crate::federation::SeedRouting`].
    pub fn routing_anchor(&self) -> IndexAnchor {
        self.anchors
            .first()
            .expect("SeedAnchorSet has at least one anchor")
            .clone()
    }
}

/// Equality `IndexScan` anchor (one property lookup via `lookup_equal`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedProbe {
    /// GQL variable to seed (e.g. `"u"` in `MATCH (u {uid: $x})`).
    pub variable: String,
    /// Property name from the plan (router catalog lookup).
    pub property: String,
    /// Interned property id for index canister calls.
    pub property_id: u32,
    /// Index key bytes for `lookup_equal` (`value_to_index_key_bytes` encoding).
    pub payload_bytes: Vec<u8>,
}

impl SeedProbe {
    /// Returns `Some` only when the anchor is a single equality `IndexScan`.
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
    ) -> Result<Option<Self>, RouterError> {
        Ok(match IndexAnchor::from_plans(plans, parameters, store)? {
            Some(IndexAnchor::Equal(probe)) => Some(probe),
            Some(IndexAnchor::Intersection { .. })
            | Some(IndexAnchor::Label { .. })
            | Some(IndexAnchor::LabelIntersection { .. })
            | None => None,
        })
    }
}

/// Extract an index anchor from a single-op prefix (`IndexScan` or `IndexIntersection`).
pub(crate) fn index_anchor_from_prefix_ops(
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    store: &RouterStore,
) -> Result<Option<IndexAnchor>, RouterError> {
    match ops {
        [] => Ok(None),
        [op] => extract_from_op(op, parameters, store),
        _ => Ok(None),
    }
}

fn resolve_vertex_label_id(store: &RouterStore, label: &str) -> Result<u32, RouterError> {
    Ok(u32::from(
        store
            .lookup_vertex_label_id(label)
            .map_err(|_| RouterError::NotFound(format!("label {label}")))?
            .raw(),
    ))
}

fn variable_from_expr(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name.as_str()),
        _ => None,
    }
}

/// Leading plan ops that establish index/label membership for one variable.
pub(crate) fn parse_seed_anchor_prefix(
    ops: &[PlanOp],
    params: &BTreeMap<String, Value>,
    store: &RouterStore,
    stats: &RouterGraphStats,
) -> Result<Option<Vec<IndexAnchor>>, RouterError> {
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
            } => Ok(Some(vec![label_anchor(store, label.as_ref(), variable)?])),
            _ => match index_anchor_from_prefix_ops(ops, params, store)? {
                Some(anchor) => Ok(Some(vec![anchor])),
                None => Ok(None),
            },
        };
    }

    let mut anchors = Vec::new();
    let mut bound_var: Option<String> = None;
    for op in ops {
        match op {
            PlanOp::NodeScan {
                label: Some(label),
                variable,
                ..
            } => {
                record_bound_var(&mut bound_var, variable)?;
                push_unique_anchor(&mut anchors, label_anchor(store, label.as_ref(), variable)?);
            }
            PlanOp::NodeScan { label: None, .. } => break,
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
                ..
            } if *cmp == CmpOp::Eq => {
                record_bound_var(&mut bound_var, variable)?;
                push_unique_anchor(
                    &mut anchors,
                    equal_anchor(store, params, variable, property.as_ref(), value)?,
                );
            }
            PlanOp::IndexIntersection {
                variable, scans, ..
            } if scans.len() >= 2 && scans.iter().all(|scan| scan.cmp == CmpOp::Eq) => {
                record_bound_var(&mut bound_var, variable)?;
                let mut specs = Vec::with_capacity(scans.len());
                for scan in scans {
                    let payload_bytes =
                        resolve_scan_value(&scan.value, params).ok_or_else(|| {
                            RouterError::InvalidArgument("missing seed parameter".into())
                        })?;
                    let property_id = store
                        .lookup_property_id(scan.property.as_ref())
                        .map_err(|_| {
                            RouterError::NotFound(format!("property {}", scan.property.as_ref()))
                        })?
                        .raw();
                    specs.push(IndexEqualSpec {
                        property_id,
                        value: payload_bytes,
                    });
                }
                push_unique_anchor(
                    &mut anchors,
                    IndexAnchor::Intersection {
                        variable: variable.to_string(),
                        specs,
                    },
                );
            }
            PlanOp::IndexScan { .. } | PlanOp::IndexIntersection { .. } => break,
            PlanOp::PropertyFilter { predicates, .. } => {
                let mut pushed = false;
                for predicate in predicates {
                    if let Some(anchor) = anchor_from_property_predicate(
                        predicate,
                        bound_var.as_deref(),
                        params,
                        store,
                        stats,
                    )? {
                        record_bound_var(&mut bound_var, anchor.variable())?;
                        if push_unique_anchor(&mut anchors, anchor) {
                            pushed = true;
                        }
                    }
                }
                if !pushed {
                    break;
                }
            }
            _ => break,
        }
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
) -> Result<(), RouterError> {
    if let Some(existing) = bound_var {
        if existing != variable.as_ref() {
            return Err(RouterError::InvalidArgument(
                "seed anchor prefix binds multiple variables".into(),
            ));
        }
    } else {
        *bound_var = Some(variable.as_ref().to_string());
    }
    Ok(())
}

fn label_anchor(
    store: &RouterStore,
    label: &str,
    variable: impl AsRef<str>,
) -> Result<IndexAnchor, RouterError> {
    Ok(IndexAnchor::Label {
        variable: variable.as_ref().to_string(),
        vertex_label_id: resolve_vertex_label_id(store, label)?,
    })
}

fn equal_anchor(
    store: &RouterStore,
    params: &BTreeMap<String, Value>,
    variable: impl AsRef<str>,
    property: &str,
    value: &ScanValue,
) -> Result<IndexAnchor, RouterError> {
    let payload_bytes = resolve_scan_value(value, params)
        .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
    let property_id = store
        .lookup_property_id(property)
        .map_err(|_| RouterError::NotFound(format!("property {property}")))?
        .raw();
    Ok(IndexAnchor::Equal(SeedProbe {
        variable: variable.as_ref().to_string(),
        property: property.to_string(),
        property_id,
        payload_bytes,
    }))
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
            Ok(Some(label_anchor(store, label, variable)?))
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
            let property_id = store
                .lookup_property_id(&property)
                .map_err(|_| RouterError::NotFound(format!("property {property}")))?
                .raw();
            Ok(Some(IndexAnchor::Equal(SeedProbe {
                variable,
                property,
                property_id,
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
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_str());
            params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))
        }
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
) -> Result<Option<IndexAnchor>, RouterError> {
    for op in ops {
        if let Some(anchor) = extract_from_op(op, parameters, store)? {
            return Ok(Some(anchor));
        }
    }
    Ok(None)
}

fn extract_from_op(
    op: &PlanOp,
    parameters: &BTreeMap<String, Value>,
    store: &RouterStore,
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
                let payload_bytes = resolve_scan_value(&scan.value, parameters)
                    .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
                let property_id = store
                    .lookup_property_id(scan.property.as_ref())
                    .map_err(|_| {
                        RouterError::NotFound(format!("property {}", scan.property.as_ref()))
                    })?
                    .raw();
                specs.push(IndexEqualSpec {
                    property_id,
                    value: payload_bytes,
                });
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
            vertex_label_id: resolve_vertex_label_id(store, label.as_ref())?,
        })),
        PlanOp::IndexScan {
            variable,
            property,
            value,
            cmp,
            ..
        } if *cmp == CmpOp::Eq => {
            let payload_bytes = resolve_scan_value(value, parameters)
                .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
            let property_id = store
                .lookup_property_id(property.as_ref())
                .map_err(|_| RouterError::NotFound(format!("property {}", property.as_ref())))?
                .raw();
            Ok(Some(IndexAnchor::Equal(SeedProbe {
                variable: variable.to_string(),
                property: property.to_string(),
                property_id,
                payload_bytes,
            })))
        }
        PlanOp::HashJoin { left, right, .. } => {
            if let Some(p) = extract_from_ops(left, parameters, store)? {
                return Ok(Some(p));
            }
            extract_from_ops(right, parameters, store)
        }
        PlanOp::CartesianProduct { left, right } => {
            if let Some(p) = extract_from_ops(left, parameters, store)? {
                return Ok(Some(p));
            }
            extract_from_ops(right, parameters, store)
        }
        PlanOp::OptionalMatch { sub_plan } => extract_from_ops(sub_plan, parameters, store),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            extract_from_ops(&sub_plan.ops, parameters, store)
        }
        PlanOp::UseGraph {
            sub_plan: Some(sp), ..
        } => extract_from_ops(sp, parameters, store),
        PlanOp::SetOperation { right, .. } => extract_from_ops(&right.ops, parameters, store),
        _ => Ok(None),
    }
}

pub(crate) fn resolve_scan_value(
    value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Option<Vec<u8>> {
    match value {
        ScanValue::Literal(v) => value_to_index_key_bytes(v).ok()?,
        ScanValue::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_ref());
            parameters
                .get(key)
                .and_then(|v| value_to_index_key_bytes(v).ok()?)
        }
    }
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
        }],
    };
    Some(Encode!(&wire).expect("SeedBindingsWire encode"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use candid::{Decode, Encode};
    use gleaph_gql::Value;
    use gleaph_gql::ast::{CmpOp, ExprKind};
    use gleaph_gql::types::LabelExpr;
    use gleaph_gql_planner::NodeLabelRef;
    use gleaph_gql_planner::PhysicalPlan;
    use gleaph_gql_planner::plan::{IndexScanSpec, PlanOp, ScanValue};
    use gleaph_graph_kernel::index::PostingHit;
    use gleaph_graph_kernel::plan_exec::SeedBindingsWire;

    use super::{IndexAnchor, SeedAnchorSet, SeedProbe, seeds_for_local_shard};
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;
    use crate::planner_stats::RouterGraphStats;

    fn test_store_with_property(property: &str) -> RouterStore {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = candid::Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        store
            .admin_intern_property(admin, property)
            .expect("intern property");
        store
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
        let store = test_store_with_property("uid");
        store
            .admin_intern_property(candid::Principal::anonymous(), "email")
            .expect("intern email");
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
        let anchor = IndexAnchor::from_plans(std::slice::from_ref(&plan), &BTreeMap::new(), &store)
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
            SeedProbe::from_plans(std::slice::from_ref(&plan), &BTreeMap::new(), &store)
                .expect("probe")
                .is_none()
        );
    }

    #[test]
    fn seed_probe_from_plans_finds_equality_index_scan() {
        let store = test_store_with_property("uid");
        let plan = index_scan_plan("uid", Value::Text("alice".into()));
        let mut params = BTreeMap::new();
        let probe = SeedProbe::from_plans(std::slice::from_ref(&plan), &params, &store)
            .expect("probe")
            .expect("some probe");
        assert_eq!(probe.variable, "u");
        assert_eq!(probe.property, "uid");
        assert_eq!(probe.property_id, 1);
        assert!(!probe.payload_bytes.is_empty());

        params.insert("x".into(), Value::Text("alice".into()));
        let plan = PhysicalPlan::from_ops(vec![PlanOp::IndexScan {
            variable: Rc::from("u"),
            property: Rc::from("uid"),
            value: ScanValue::Parameter(Rc::from("$x")),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);
        let probe = SeedProbe::from_plans(std::slice::from_ref(&plan), &params, &store)
            .expect("probe")
            .expect("parameter probe");
        assert!(!probe.payload_bytes.is_empty());
    }

    #[test]
    fn index_anchor_from_plans_finds_multi_label_intersection() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = candid::Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        store
            .admin_intern_vertex_label(admin, "Person")
            .expect("intern Person");
        store
            .admin_intern_vertex_label(admin, "Employee")
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
        let anchor = IndexAnchor::from_plans(std::slice::from_ref(&plan), &BTreeMap::new(), &store)
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
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = candid::Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        store
            .admin_intern_vertex_label(admin, "Person")
            .expect("intern Person");
        store
            .admin_intern_property(admin, "region")
            .expect("intern region");
        let stats = RouterGraphStats::default().with_indexed_vertex_property("region");
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
        assert_eq!(set.variable, "n");
        assert_eq!(set.anchors.len(), 2);
        assert!(set.anchors.iter().any(|anchor| {
            matches!(
                anchor,
                IndexAnchor::Label {
                    vertex_label_id: 1,
                    ..
                }
            )
        }));
        assert!(
            set.anchors
                .iter()
                .any(|anchor| matches!(anchor, IndexAnchor::Equal(_)))
        );
    }

    #[test]
    fn index_anchor_from_plans_finds_labeled_node_scan() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = candid::Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        store
            .admin_intern_vertex_label(admin, "Person")
            .expect("intern Person");
        let plan = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
            variable: Rc::from("n"),
            label: Some(NodeLabelRef::from("Person")),
            property_projection: None,
        }]);
        let anchor = IndexAnchor::from_plans(std::slice::from_ref(&plan), &BTreeMap::new(), &store)
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
                shard_id: 7,
                vertex_id: 10,
            },
            PostingHit {
                shard_id: 9,
                vertex_id: 20,
            },
            PostingHit {
                shard_id: 7,
                vertex_id: 11,
            },
        ];
        let blob = seeds_for_local_shard("u", &hits, 7).expect("seed blob");
        let wire: SeedBindingsWire = Decode!(&blob, SeedBindingsWire).expect("decode");
        assert_eq!(wire.entries.len(), 1);
        assert_eq!(wire.entries[0].variable, "u");
        assert_eq!(wire.entries[0].local_vertex_ids, vec![10, 11]);

        let roundtrip: SeedBindingsWire =
            Decode!(&Encode!(&wire).expect("re-encode"), SeedBindingsWire).expect("roundtrip");
        assert_eq!(wire, roundtrip);
        assert!(seeds_for_local_shard("u", &hits, 99).is_none());
    }
}
