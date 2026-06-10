//! Request bodies for canister methods (called from `lib.rs` ic-cdk entrypoints).

use std::collections::BTreeMap;

use crate::facade::{FederationRouting, GraphMetadata, GraphStore};
use crate::gql_execution_context::GqlExecutionContext;
use crate::gql_run::{kernel_execution_mode, run_wire_plan_last_read_row_count};
use crate::index::ic::IcPropertyIndexClient;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::router::verify_shard_attachment;
use candid::Decode;
use gleaph_gql::Value;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_graph_kernel::federation::{FederatedExpandArgs, FederatedExpandNeighbor};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, GqlExecutionMode, SeedBindingsWire,
};

use super::types::GraphInitArgs;

pub async fn init(args: GraphInitArgs) {
    let federation_routing = match (args.router_canister, args.shard_id) {
        (Some(router_canister), Some(shard_id)) => {
            #[cfg(target_family = "wasm")]
            let entry = verify_shard_attachment(
                router_canister,
                shard_id,
                args.logical_graph_name.as_deref(),
            )
            .await
            .unwrap_or_else(|e| ic_cdk::trap(e.to_string()));
            #[cfg(not(target_family = "wasm"))]
            let entry = verify_shard_attachment(
                router_canister,
                shard_id,
                args.logical_graph_name.as_deref(),
            )
            .unwrap_or_else(|e| panic!("{e}"));
            Some(FederationRouting {
                router_canister,
                shard_id,
                index_canister: entry.index_canister,
            })
        }
        (None, None) => None,
        _ => ic_cdk::trap(
            "GraphInitArgs: router_canister and shard_id must both be set or both omitted",
        ),
    };

    let mut metadata = GraphMetadata::default();
    metadata.set_logical_graph_name(args.logical_graph_name);
    metadata.set_federation_routing(federation_routing);

    if let Err(err) = GraphStore::new().set_metadata(metadata) {
        ic_cdk::trap(err.to_string());
    }
}

pub(crate) fn decode_gql_param_map(params: Vec<u8>) -> Result<BTreeMap<String, Value>, String> {
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = canbench_rs::bench_scope("gql_ic_params_blob_decode");
    decode_gql_params_blob(&params).map_err(|e| e.to_string())
}

fn wasm_index_client_holder() -> Option<IcPropertyIndexClient> {
    GraphStore::new()
        .federation_routing()
        .map(|r| IcPropertyIndexClient {
            index_principal: r.index_canister,
            shard_id: r.shard_id,
        })
}

fn ensure_execution_mode(
    args_mode: GqlExecutionMode,
    expected: GqlExecutionMode,
    entrypoint: &str,
) -> Result<(), String> {
    if args_mode != expected {
        return Err(format!(
            "{entrypoint} requires {expected:?} mode (got {args_mode:?})"
        ));
    }
    Ok(())
}

pub async fn execute_plan_query(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    ensure_execution_mode(args.mode, GqlExecutionMode::Query, "execute_plan_query")?;
    execute_plan_impl(args).await
}

pub async fn execute_plan_update(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    ensure_execution_mode(args.mode, GqlExecutionMode::Update, "execute_plan_update")?;
    execute_plan_impl(args).await
}

async fn execute_plan_impl(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    let store = GraphStore::new();
    let routing = store
        .federation_routing()
        .ok_or("federation routing not configured")?;
    if routing.shard_id != args.target_shard_id {
        return Err(format!(
            "target_shard_id {} does not match this graph shard {}",
            args.target_shard_id, routing.shard_id
        ));
    }
    let pmap = decode_gql_param_map(args.params_blob)?;
    let seeds = match args.seed_bindings_blob {
        Some(blob) => {
            let wire: SeedBindingsWire = Decode!(&blob, SeedBindingsWire)
                .map_err(|e| format!("seed_bindings decode: {e}"))?;
            Some(wire)
        }
        None => None,
    };
    // Router-owned index anchors: federated graph shards must not call index on read path.
    #[cfg(target_family = "wasm")]
    let index_holder = if seeds.is_some() || store.federation_routing().is_some() {
        None
    } else {
        wasm_index_client_holder()
    };
    #[cfg(target_family = "wasm")]
    let ix = index_holder.as_ref().map(|c| c as &dyn PropertyIndexLookup);
    #[cfg(not(target_family = "wasm"))]
    let ix: Option<&dyn PropertyIndexLookup> = None;

    let run = run_wire_plan_last_read_row_count(
        store,
        &args.plan_blob,
        &pmap,
        kernel_execution_mode(args.mode),
        ix,
        GqlExecutionContext {
            caller: None,
            resolved_labels: args.resolved_labels,
        },
        seeds,
        args.mutation_id,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(ExecutePlanResult {
        row_count: run.row_count as u64,
        label_telemetry_events: run.label_telemetry_events,
        rows_blob: run.rows_blob,
    })
}

pub fn ack_label_telemetry_event(seq: gleaph_graph_kernel::plan_exec::ShardEventSeq) {
    GraphStore::new().ack_label_telemetry_event(seq);
}

pub fn list_pending_label_telemetry_events(
    from_seq: gleaph_graph_kernel::plan_exec::ShardEventSeq,
    limit: u32,
) -> Vec<gleaph_graph_kernel::plan_exec::LabelTelemetryEventWire> {
    GraphStore::new().pending_label_telemetry_events(from_seq, limit)
}

pub fn get_mutation_outcome(
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
) -> Option<gleaph_graph_kernel::plan_exec::MutationOutcomeWire> {
    GraphStore::new().mutation_outcome(mutation_id)
}

pub fn bootstrap_graph_peers(
    args: gleaph_graph_kernel::federation::BootstrapGraphPeersArgs,
) -> Result<(), String> {
    let self_canister = ic_cdk::api::canister_self();
    GraphStore::new().bootstrap_peer_graph_canisters(&args.peers, self_canister);
    Ok(())
}

pub fn add_graph_peer(
    args: gleaph_graph_kernel::federation::AddGraphPeerArgs,
) -> Result<(), String> {
    let self_canister = ic_cdk::api::canister_self();
    GraphStore::new().add_peer_graph_canister(args.peer, self_canister);
    Ok(())
}

pub fn remove_graph_peer(
    args: gleaph_graph_kernel::federation::RemoveGraphPeerArgs,
) -> Result<(), String> {
    GraphStore::new().remove_peer_graph_canister(&args.peer);
    Ok(())
}

#[cfg(feature = "pocket-ic-e2e")]
pub fn e2e_attach_federation(args: super::types::E2eAttachFederationArgs) -> Result<(), String> {
    if ic_cdk::api::msg_caller() != args.router_canister {
        return Err("e2e_attach_federation: caller must be the configured router".into());
    }
    let mut metadata = GraphMetadata::default();
    metadata.set_logical_graph_name(args.logical_graph_name);
    metadata.set_federation_routing(Some(FederationRouting {
        router_canister: args.router_canister,
        shard_id: args.shard_id,
        index_canister: args.index_canister,
    }));
    GraphStore::new()
        .set_metadata(metadata)
        .map_err(|e| e.to_string())
}

#[cfg(feature = "pocket-ic-e2e")]
pub async fn e2e_insert_vertex() -> Result<super::types::E2eInsertVertexResult, String> {
    use crate::index::placement;
    let store = GraphStore::new();
    let vertex_id = store
        .insert_vertex_row(gleaph_graph_kernel::entry::Vertex::default())
        .await
        .map_err(|e| e.to_string())?;
    let logical_vertex_id = store
        .logical_vertex_id(vertex_id)
        .ok_or_else(|| "logical id missing after insert".to_string())?;
    Ok(super::types::E2eInsertVertexResult {
        local_vertex_id: placement::local_vertex_id_raw(vertex_id),
        logical_vertex_id,
    })
}

#[cfg(feature = "pocket-ic-e2e")]
pub fn e2e_insert_directed_edge(
    args: super::types::E2eInsertDirectedEdgeArgs,
) -> Result<(), String> {
    let store = GraphStore::new();
    let source = ic_stable_lara::VertexId::from(args.source_local_vertex_id);
    let target = ic_stable_lara::VertexId::from(args.target_local_vertex_id);
    store
        .insert_directed_edge(source, target, None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn federated_expand(
    args: FederatedExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, String> {
    let store = GraphStore::new();
    crate::facade::federation_expand::collect_federated_expand(&store, args)
        .await
        .map_err(|e| e.to_string())
}

pub async fn backfill_label_postings(
    args: gleaph_graph_kernel::federation::PostingBackfillArgs,
) -> Result<gleaph_graph_kernel::federation::PostingBackfillResult, String> {
    let store = GraphStore::new();
    let Some(index) = wasm_index_client_holder() else {
        return Err("federation not configured".into());
    };
    crate::index::label_backfill::backfill_label_postings(&store, &index, args).await
}

pub async fn backfill_property_postings(
    args: gleaph_graph_kernel::federation::PostingBackfillArgs,
) -> Result<gleaph_graph_kernel::federation::PostingBackfillResult, String> {
    let store = GraphStore::new();
    let Some(index) = wasm_index_client_holder() else {
        return Err("federation not configured".into());
    };
    crate::index::property_backfill::backfill_property_postings(&store, &index, args).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Encode, Principal};
    use gleaph_gql::Value;
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql::types::LabelExpr;
    use gleaph_gql_ic::encode_gql_params_blob;
    use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn};
    use gleaph_gql_planner::wire::encode_block_plans;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::plan_exec::{SeedBindingEntry, SeedBindingsWire};
    use pollster;

    const TEST_SHARD_ID: ShardId = 7;

    fn attach_test_federation(shard_id: ShardId) {
        let mut metadata = GraphMetadata::default();
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            shard_id,
        }));
        GraphStore::new()
            .set_metadata(metadata)
            .expect("attach federation routing");
    }

    fn label_intersection_plan_with_seeds(
        store: &GraphStore,
        person_label: &str,
        employee_label: &str,
    ) -> (Vec<u8>, Vec<u8>, u32) {
        let vid = store
            .insert_vertex_named([person_label, employee_label], Vec::<(&str, Value)>::new())
            .expect("vertex with both labels");
        let _person_only = store
            .insert_vertex_named([person_label], Vec::<(&str, Value)>::new())
            .expect("person only");
        let local_vid = u32::try_from(u64::from(vid)).expect("local vertex id");
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some(person_label.into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::IsLabeled {
                    expr: Box::new(Expr::new(ExprKind::Variable("n".into()))),
                    label: LabelExpr::Name(employee_label.into()),
                    negated: false,
                })],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".into())),
                    alias: Some("n".into()),
                }],
                distinct: false,
            },
        ]);
        let plan_blob = encode_block_plans(&[plan], false).expect("encode plan");
        let seeds = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "n".into(),
                local_vertex_ids: vec![local_vid],
            }],
        };
        let seed_blob = Encode!(&seeds).expect("encode seeds");
        (plan_blob, seed_blob, local_vid)
    }

    #[test]
    fn execute_plan_query_seed_bindings_skip_label_intersection() {
        attach_test_federation(TEST_SHARD_ID);
        let store = GraphStore::new();
        let (plan_blob, seed_blob, _local_vid) =
            label_intersection_plan_with_seeds(&store, "HandlerSeedPerson", "HandlerSeedEmployee");
        let params_blob = encode_gql_params_blob(vec![]).expect("encode params");
        let args = ExecutePlanArgs {
            target_shard_id: TEST_SHARD_ID,
            mutation_id: None,
            plan_blob,
            params_blob,
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: None,
        };

        let result = pollster::block_on(execute_plan_query(args)).expect("execute_plan_query");

        assert_eq!(result.row_count, 1);
        let rows_blob = result.rows_blob.expect("query rows_blob");
        let wire = gleaph_gql_ic::IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode");
        let materialized =
            crate::plan::plan_query_result_from_ic_wire(wire).expect("materialize rows");
        assert_eq!(materialized.rows.len(), 1);
        assert!(materialized.rows[0].contains_key("n"));
    }

    #[test]
    fn execute_plan_query_rejects_shard_mismatch() {
        attach_test_federation(TEST_SHARD_ID);
        let store = GraphStore::new();
        let (plan_blob, seed_blob, _) = label_intersection_plan_with_seeds(
            &store,
            "HandlerShardPerson",
            "HandlerShardEmployee",
        );
        let params_blob = encode_gql_params_blob(vec![]).expect("encode params");
        let args = ExecutePlanArgs {
            target_shard_id: TEST_SHARD_ID + 1,
            mutation_id: None,
            plan_blob,
            params_blob,
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: None,
        };

        let err = pollster::block_on(execute_plan_query(args)).expect_err("shard mismatch");
        assert!(
            err.contains("target_shard_id") && err.contains("does not match"),
            "unexpected error: {err}"
        );
    }
}
