//! Manual PocketIC probe for complete-row seed memory scaling.
//!
//! This is intentionally ignored: it probes progressively larger Candid requests and is not a
//! correctness test. It records request bytes, canister wasm memory, and the first rejection/trap
//! so a fixed seed-row bound is not chosen from a host-only allocation estimate.

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql::ast::CmpOp;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn, ScanValue};
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, GqlExecutionMode, SeedBindingsWire, SeedRowWire,
    SeedVertexBinding,
};
use gleaph_pocket_ic_tests::{
    SOURCE_SHARD, e2e_insert_vertex_with_property, ensure_property,
    federation_graph_element_id_encoding_key_bytes, install_single_shard_federation,
};
use std::rc::Rc;

fn probe_plan_blob() -> Vec<u8> {
    let plan = PhysicalPlan::from_ops(vec![
        PlanOp::IndexScan {
            variable: Rc::from("n"),
            property: Rc::from("probe_key"),
            value: ScanValue::Literal(Value::Int64(1)),
            cmp: CmpOp::Eq,
            property_projection: None,
        },
        PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: gleaph_gql::ast::Expr::var("n"),
                alias: Some(Rc::from("n")),
            }],
            distinct: false,
        },
    ]);
    encode_block_plans(std::slice::from_ref(&plan), false).expect("encode probe plan")
}

fn complete_seed_rows(count: usize, local_vertex_id: u32) -> SeedBindingsWire {
    SeedBindingsWire {
        entries: Vec::new(),
        rows: (0..count)
            .map(|_| SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "n".into(),
                    local_vertex_id,
                    required_vertex_label_ids: Vec::new(),
                }],
                float64_bindings: Vec::new(),
            })
            .collect(),
        complete_prefix_rows: true,
    }
}

fn wasm_memory_size(env: &gleaph_pocket_ic_tests::FederationEnv, canister: Principal) -> String {
    env.pic
        .canister_status(canister, None)
        .expect("canister status")
        .memory_metrics
        .wasm_memory_size
        .to_string()
}

#[test]
#[ignore = "manual memory probe; run explicitly"]
fn complete_seed_rows_stop_at_message_or_heap_boundary() {
    let env = install_single_shard_federation();
    let plan_blob = probe_plan_blob();
    let element_id_encoding_key = federation_graph_element_id_encoding_key_bytes(&env);
    let property_id = ensure_property(&env, "probe_key");
    let vertex = e2e_insert_vertex_with_property(&env, env.graph_source, property_id.raw(), 1);

    // The final sizes intentionally approach the IC message ceiling. If message admission stops
    // first, that is evidence that heap exhaustion was not the active constraint for this shape.
    for rows in [
        128usize, 512, 1_024, 4_096, 16_384, 65_536, 131_072, 196_608, 220_000, 240_000, 280_000,
        320_000, 360_000, 400_000, 440_000,
    ] {
        let seed_blob =
            Encode!(&complete_seed_rows(rows, vertex.local_vertex_id)).expect("encode probe seeds");
        let args = ExecutePlanArgs {
            target_shard_id: SOURCE_SHARD,
            element_id_encoding_key,
            mutation_id: None,
            plan_blob: plan_blob.clone(),
            params_blob: Vec::new(),
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
            local_unique_claims: None,
            local_constrained_properties: None,
            indexed_embeddings: None,
            resolved_search_blob: None,
        };
        let request_bytes = Encode!(&args).expect("encode probe args");
        let before = wasm_memory_size(&env, env.graph_source);
        let result = env.pic.query_call(
            env.graph_source,
            env.router,
            "execute_plan_query",
            request_bytes.clone(),
        );
        let after = wasm_memory_size(&env, env.graph_source);
        let outcome = match result {
            Ok(bytes) => match Decode!(&bytes, Result<ExecutePlanResult, String>) {
                Ok(Ok(_)) => "ok".to_string(),
                Ok(Err(error)) => format!("graph_error:{error}"),
                Err(error) => format!("decode_error:{error}"),
            },
            Err(error) => format!("transport_reject:{error}"),
        };
        println!(
            "seed_heap_probe rows={rows} request_bytes={} wasm_memory_before={} wasm_memory_after={} result={outcome}",
            request_bytes.len(),
            before,
            after,
        );
        if outcome != "ok" {
            break;
        }
    }
}
