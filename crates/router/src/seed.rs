//! Index anchor detection and per-shard seed binding for graph dispatch.
//!
//! The router resolves query entry points via the property index before calling graph
//! shards. [`IndexAnchor`] captures the leading index op (`IndexScan` or
//! `IndexIntersection`); hits are encoded into `seed_bindings_blob` so shards can skip
//! that op.

use std::collections::BTreeMap;

use candid::Encode;
use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::plan::{PlanOp, ScanValue};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{IndexEqualSpec, PostingHit};
use gleaph_graph_kernel::plan_exec::{SeedBindingEntry, SeedBindingsWire};

use crate::facade::store::RouterStore;
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
}

impl IndexAnchor {
    /// Variable bound by this anchor (also used in `SeedBindingsWire`).
    pub fn variable(&self) -> &str {
        match self {
            Self::Equal(probe) => probe.variable.as_str(),
            Self::Intersection { variable, .. } => variable.as_str(),
        }
    }

    /// Scan physical plans for the first index anchor (`IndexIntersection` or equality `IndexScan`).
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
    ) -> Result<Option<Self>, RouterError> {
        for plan in plans {
            if let Some(anchor) = extract_from_ops(&plan.ops, parameters, store)? {
                return Ok(Some(anchor));
            }
        }
        Ok(None)
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
            Some(IndexAnchor::Intersection { .. }) | None => None,
        })
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

fn resolve_scan_value(value: &ScanValue, parameters: &BTreeMap<String, Value>) -> Option<Vec<u8>> {
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
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql_planner::PhysicalPlan;
    use gleaph_gql_planner::plan::{IndexScanSpec, PlanOp, ScanValue};
    use gleaph_graph_kernel::index::PostingHit;
    use gleaph_graph_kernel::plan_exec::SeedBindingsWire;

    use super::{IndexAnchor, SeedProbe, seeds_for_local_shard};
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;

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
