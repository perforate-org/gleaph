//! Router-side GQL parse, plan, index seed routing, and graph dispatch.

use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::build_block_plan_with_schema;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::PostingHit;
use gleaph_graph_kernel::plan_exec::GqlExecutionMode;
use ic_cdk::api::msg_caller;

use crate::execution_path::check_adhoc_execution_path;
use crate::facade::store::RouterStore;
use crate::graph_client::execute_plan_on_graph;
use crate::index_client::RouterIndexClient;
use crate::planner_stats::RouterGraphStats;
use crate::rbac::authorize_adhoc_gql;
use crate::seed::{SeedProbe, seeds_for_local_shard};
use crate::state::RouterError;

pub async fn gql_query(
    logical_graph_name: String,
    query: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    run_gql(
        &logical_graph_name,
        &query,
        &params,
        GqlExecutionMode::Query,
        "gql_query",
        false,
    )
    .await
}

pub async fn gql_execute(
    logical_graph_name: String,
    query: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    run_gql(
        &logical_graph_name,
        &query,
        &params,
        GqlExecutionMode::Update,
        "gql_execute",
        false,
    )
    .await
}

/// Run a read-only program on the **update** path (higher cost; escape hatch only).
pub async fn force_gql_execute(
    logical_graph_name: String,
    query: String,
    params: Vec<u8>,
) -> Result<u64, RouterError> {
    run_gql(
        &logical_graph_name,
        &query,
        &params,
        GqlExecutionMode::Update,
        "force_gql_execute",
        true,
    )
    .await
}

async fn run_gql(
    logical_graph_name: &str,
    query: &str,
    params: &[u8],
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
) -> Result<u64, RouterError> {
    let program = parser::parse(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let flags = classify_program(&program);
    let caller = msg_caller();
    authorize_adhoc_gql(&caller, flags)?;
    check_adhoc_execution_path(entrypoint, mode, flags, force)?;

    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing statement block".into()))?;

    let stats = RouterGraphStats::for_graph(logical_graph_name);
    let plan = build_block_plan_with_schema(block, Some(&stats), &NoSchema)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let requires_write_path = plan.has_dml();
    if requires_write_path != flags.requires_write_path() {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
    }
    let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    let pmap =
        decode_gql_params_blob(params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    dispatch_plan_blob(
        logical_graph_name,
        &plan_blob,
        std::slice::from_ref(&plan),
        &pmap,
        params,
        mode,
    )
    .await
}

/// Route and execute a plan blob (single- or multi-shard).
pub async fn dispatch_plan_blob(
    logical_graph_name: &str,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
) -> Result<u64, RouterError> {
    let store = RouterStore::new();
    let shards = store.list_shards_for_graph(logical_graph_name)?;
    if shards.is_empty() {
        return Err(RouterError::ShardNotRegistered);
    }
    let resolved_labels = store.resolve_plan_labels(plans)?;
    let index = RouterIndexClient::new(shards[0].index_canister);
    let seed_probe = SeedProbe::from_plans(plans, pmap, &store)?;

    let routings = match seed_probe {
        Some(probe) => {
            let hits = index
                .lookup_equal(probe.property_id, probe.payload_bytes.clone())
                .await
                .map_err(RouterError::InvalidArgument)?;
            if hits.is_empty() {
                return Ok(0);
            }
            resolve_seed_routings_multi(&store, &hits, logical_graph_name, probe)?
        }
        None => {
            if shards.len() != 1 {
                return Err(RouterError::InvalidArgument(
                    "no index anchor: single-shard graph required".into(),
                ));
            }
            vec![SeedRouting {
                shard_id: shards[0].shard_id,
                graph_canister: shards[0].graph_canister,
                hits: Vec::new(),
                probe: None,
            }]
        }
    };

    let mut total_rows = 0u64;
    for routing in routings {
        let seed_blob = routing.probe.as_ref().and_then(|probe| {
            seeds_for_local_shard(probe.variable.as_str(), &routing.hits, routing.shard_id)
        });
        let result = execute_plan_on_graph(
            routing.graph_canister,
            gleaph_graph_kernel::plan_exec::ExecutePlanArgs {
                target_shard_id: routing.shard_id,
                plan_blob: plan_blob.to_vec(),
                params_blob: params.to_vec(),
                mode,
                seed_bindings_blob: seed_blob,
                resolved_labels: Some(resolved_labels.clone()),
            },
        )
        .await
        .map_err(RouterError::InvalidArgument)?;
        total_rows = total_rows.saturating_add(result.row_count);
    }
    Ok(total_rows)
}

#[derive(Clone, Debug)]
pub struct SeedRouting {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub hits: Vec<PostingHit>,
    pub probe: Option<SeedProbe>,
}

/// Phase 4: fan out one routing per distinct shard in index hits.
pub fn resolve_seed_routings_multi(
    store: &RouterStore,
    hits: &[PostingHit],
    logical_graph_name: &str,
    probe: SeedProbe,
) -> Result<Vec<SeedRouting>, RouterError> {
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let shards = store.list_shards_for_graph(logical_graph_name)?;
    let mut shard_ids: Vec<ShardId> = hits.iter().map(|h| h.shard_id).collect();
    shard_ids.sort_unstable();
    shard_ids.dedup();

    let mut out = Vec::with_capacity(shard_ids.len());
    for shard_id in shard_ids {
        let entry = shards
            .iter()
            .find(|s| s.shard_id == shard_id)
            .ok_or(RouterError::ShardNotRegistered)?;
        let shard_hits: Vec<PostingHit> = hits
            .iter()
            .filter(|h| h.shard_id == shard_id)
            .cloned()
            .collect();
        out.push(SeedRouting {
            shard_id,
            graph_canister: entry.graph_canister,
            hits: shard_hits,
            probe: Some(probe.clone()),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use candid::Principal;
    use gleaph_graph_kernel::index::PostingHit;

    use crate::facade::store::RouterStore;
    use crate::gql::resolve_seed_routings_multi;
    use crate::init::RouterInitArgs;
    use crate::seed::SeedProbe;
    use crate::state::RouterError;
    use crate::types::AdminRegisterShardArgs;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn store_with_shards() -> RouterStore {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        for (shard_id, graph_byte) in [(7u32, 1u8), (9, 4)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph_principal(graph_byte),
                    index_canister: graph_principal(2),
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register shard");
        }
        store
    }

    #[test]
    fn resolve_seed_routings_multi_fans_out_by_shard() {
        let store = store_with_shards();
        let probe = SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            payload_bytes: vec![1, 2, 3],
        };
        let hits = vec![
            PostingHit {
                shard_id: 7,
                vertex_id: 10,
            },
            PostingHit {
                shard_id: 9,
                vertex_id: 20,
            },
        ];
        let routings = resolve_seed_routings_multi(&store, &hits, "tenant.main", probe.clone())
            .expect("route");
        assert_eq!(routings.len(), 2);
        assert_eq!(routings[0].shard_id, 7);
        assert_eq!(routings[1].shard_id, 9);
        assert_eq!(routings[0].hits.len(), 1);
        assert_eq!(routings[0].hits[0].vertex_id, 10);
        assert!(routings[0].probe.as_ref().is_some());
        assert_eq!(routings[0].graph_canister, graph_principal(1));
    }

    #[test]
    fn resolve_seed_routings_multi_rejects_unknown_shard() {
        let store = store_with_shards();
        let probe = SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            payload_bytes: vec![],
        };
        let hits = vec![PostingHit {
            shard_id: 99,
            vertex_id: 1,
        }];
        let err = resolve_seed_routings_multi(&store, &hits, "tenant.main", probe)
            .expect_err("unknown shard");
        assert!(matches!(err, RouterError::ShardNotRegistered));
    }
}
