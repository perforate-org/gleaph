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
use crate::gql::SeedRouting;
use crate::state::RouterError;

#[derive(Clone, Debug)]
pub struct SeedProbe {
    pub variable: String,
    pub property: String,
    pub property_id: u32,
    pub value_bytes: Vec<u8>,
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
            let value_bytes = resolve_scan_value(value, parameters)
                .ok_or_else(|| RouterError::InvalidArgument("missing seed parameter".into()))?;
            let property_id = store
                .lookup_property_id(property.as_ref())
                .map_err(|_| RouterError::NotFound(format!("property {}", property.as_ref())))?
                .raw();
            return Ok(Some(SeedProbe {
                variable: variable.to_string(),
                property: property.to_string(),
                property_id,
                value_bytes,
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

pub fn resolve_seed_shard_with_probe(
    store: &RouterStore,
    hits: &[PostingHit],
    logical_graph_name: &str,
    probe: SeedProbe,
) -> Result<crate::gql::SeedRouting, RouterError> {
    let mut routings =
        crate::gql::resolve_seed_routings_multi(store, hits, logical_graph_name, probe)?;
    if routings.len() != 1 {
        return Err(RouterError::InvalidArgument(format!(
            "expected one shard, got {}",
            routings.len()
        )));
    }
    Ok(routings.remove(0))
}
