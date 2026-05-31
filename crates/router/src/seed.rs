//! Seed vertex resolution via property index + placement.

use std::collections::BTreeMap;

use candid::Encode;
use gleaph_gql::Value;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::plan::{PlanOp, ScanValue};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::PostingHit;
use gleaph_graph_kernel::plan_exec::{SeedBindingEntry, SeedBindingsWire};

use crate::facade::store::RouterStore;
use crate::state::RouterError;

#[derive(Clone, Debug, PartialEq)]
pub struct SeedProbe {
    pub variable: String,
    pub property: String,
    pub property_id: u32,
    pub payload_bytes: Vec<u8>,
}

impl SeedProbe {
    pub fn from_plans(
        plans: &[PhysicalPlan],
        parameters: &BTreeMap<String, Value>,
        store: &RouterStore,
    ) -> Result<Option<Self>, RouterError> {
        for plan in plans {
            if let Some(probe) = extract_from_ops(&plan.ops, parameters, store)? {
                return Ok(Some(probe));
            }
        }
        Ok(None)
    }
}

fn extract_from_ops(
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    store: &RouterStore,
) -> Result<Option<SeedProbe>, RouterError> {
    for op in ops {
        if let Some(probe) = extract_from_op(op, parameters, store)? {
            return Ok(Some(probe));
        }
    }
    Ok(None)
}

fn extract_from_op(
    op: &PlanOp,
    parameters: &BTreeMap<String, Value>,
    store: &RouterStore,
) -> Result<Option<SeedProbe>, RouterError> {
    match op {
        PlanOp::IndexScan {
            variable,
            property,
            value,
            cmp,
            ..
        } if *cmp == gleaph_gql::ast::CmpOp::Eq => {
            let payload_bytes = resolve_scan_value(value, parameters)
                .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
            let property_id = store
                .lookup_property_id(property.as_ref())
                .map_err(|_| RouterError::NotFound(format!("property {}", property.as_ref())))?
                .raw();
            return Ok(Some(SeedProbe {
                variable: variable.to_string(),
                property: property.to_string(),
                property_id,
                payload_bytes,
            }));
        }
        PlanOp::HashJoin { left, right, .. } => {
            if let Some(p) = extract_from_ops(left, parameters, store)? {
                return Ok(Some(p));
            }
            return extract_from_ops(right, parameters, store);
        }
        PlanOp::CartesianProduct { left, right } => {
            if let Some(p) = extract_from_ops(left, parameters, store)? {
                return Ok(Some(p));
            }
            return extract_from_ops(right, parameters, store);
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
    use gleaph_gql_planner::plan::{PlanOp, ScanValue};
    use gleaph_graph_kernel::index::PostingHit;
    use gleaph_graph_kernel::plan_exec::{SeedBindingEntry, SeedBindingsWire};

    use super::{SeedProbe, seeds_for_local_shard};
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
