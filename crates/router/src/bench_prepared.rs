//! Prepared sorted-variant cache benchmarks (GAP-2026-07-31-001).
//!
//! Measures the two shapes of a sorted prepared-query execution:
//! - `bench_prepared_sorted_variant_rebuild`: the miss path, which validates the
//!   sort specification, injects the `ORDER BY`, re-plans through the production
//!   planning seam, and encodes the plan blob. This is what every sort-enabled
//!   execution paid before the heap cache existed.
//! - `bench_prepared_sorted_variant_cache_hit`: the hit path, which normalizes
//!   the signature, looks up the bounded heap map, and clones the derived plan.
//!
//! Run from `crates/router`: `canbench sorted_variant`.

use crate::facade::stable::prepared_catalog::{
    PreparedPlanKey, PreparedPlanRecord, PreparedPlanRecordV1, insert_prepared_plan,
};
use crate::facade::store::RouterStore;
use crate::prepared::{
    build_prepared_cache, prepare_sorted_cache, prepared_sorted_cache_for_execution,
};
use canbench_rs::bench;
use candid::Principal;
use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_prepared_api::{
    Column, OperationKind, PreparedOperation, PreparedSortSpec, ResultSchema, SemanticType, SortKey,
};
use std::hint::black_box;

struct SortedVariantFixture {
    owner: Principal,
    graph_id: GraphId,
    key: PreparedPlanKey,
    metadata: PreparedOperation,
    base: crate::prepared::PreparedQueryCache,
}

fn sorted_variant_fixture() -> SortedVariantFixture {
    let store = RouterStore::new();
    let owner = Principal::from_slice(&[21u8, 7, 3, 0, 0, 0, 0, 0, 0]);
    let graph_id = GraphId::from_raw(4401);
    crate::facade::auth::grant_admins(&[owner]);
    store
        .admin_register_graph(
            owner,
            GraphRegistryEntry {
                graph_id,
                canister_id: Principal::management_canister(),
                owner,
                admins: Default::default(),
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::None,
                is_home: false,
            },
            "bench-sorted-variant",
        )
        .expect("register bench graph");
    let metadata = PreparedOperation {
        name: "bench-sorted".into(),
        description: None,
        kind: OperationKind::Query,
        parameters: vec![],
        result: ResultSchema {
            columns: vec![Column {
                name: "n".into(),
                semantic_type: SemanticType::Text,
                nullable: false,
            }],
        },
        supports_consistency: false,
        supports_idempotency: false,
        allowed_sorts: vec![SortKey {
            key: "n".into(),
            label: None,
        }],
    };
    let query = "MATCH (n:BenchSortedLabel) RETURN n";
    let key = PreparedPlanKey::new("bench-sorted");
    insert_prepared_plan(
        key.clone(),
        PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
            graph_id,
            query: query.to_string(),
            metadata: Some(metadata.clone()),
        }),
    );
    let (base, planned) = build_prepared_cache(query, owner, Some(graph_id)).expect("base plan");
    assert_eq!(planned, graph_id);
    SortedVariantFixture {
        owner,
        graph_id,
        key,
        metadata,
        base,
    }
}

fn bench_sort_specs() -> Vec<PreparedSortSpec> {
    vec![PreparedSortSpec {
        key: "n".into(),
        direction: "ascending".into(),
    }]
}

#[bench(raw)]
fn bench_prepared_sorted_variant_rebuild() -> canbench_rs::BenchResult {
    let fixture = sorted_variant_fixture();
    let specs = bench_sort_specs();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("prepared_sorted_variant_rebuild");
        let derived = prepare_sorted_cache(
            &fixture.base,
            Some(&fixture.metadata),
            black_box(&specs),
            fixture.owner,
            fixture.graph_id,
        )
        .expect("rebuild sorted variant");
        black_box(derived.plan_blob.len());
    })
}

#[bench(raw)]
fn bench_prepared_sorted_variant_cache_hit() -> canbench_rs::BenchResult {
    let fixture = sorted_variant_fixture();
    let specs = bench_sort_specs();
    // Seed the cache so the measured closure is the pure hit path.
    prepared_sorted_cache_for_execution(
        &fixture.base,
        &fixture.key,
        Some(&fixture.metadata),
        &specs,
        fixture.owner,
        fixture.graph_id,
    )
    .expect("seed sorted variant");
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("prepared_sorted_variant_cache_hit");
        let cached = prepared_sorted_cache_for_execution(
            &fixture.base,
            &fixture.key,
            Some(&fixture.metadata),
            black_box(&specs),
            fixture.owner,
            fixture.graph_id,
        )
        .expect("hit sorted variant");
        black_box(cached.plan_blob.len());
    })
}
