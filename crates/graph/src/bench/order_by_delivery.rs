//! ADR 0081 Slice A ordered-delivery benchmarks: index-ordered TopK versus the
//! Sort-remaining fallback over an identical fixture.
//!
//! Both arms plan the same query (`WHERE v.score >= 0 AND v.flag = 0 ... ORDER BY
//! score LIMIT 10`), then one strips the `ordered_by_sort` intent from the leading
//! [`PlanOp::IndexScan`]. With the intent the executor's TopK stops consuming at the
//! first strictly greater key past the boundary survivor; without it the same TopK
//! full-sorts every survivor. The instruction delta therefore measures exactly the
//! ordered-delivery executor win at fixed data shape (512 indexed rows, 256
//! survivors, k = 10).
//!
//! Index hits are served by an in-memory range-only lookup so the measured closure
//! isolates delivery cost from stable-storage posting walks.

use std::collections::BTreeMap;
use std::hint::black_box;

use async_trait::async_trait;
use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::parser;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::{PlanBuildOptions, build_plan_with_schema_and_options};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{PhysicalIndexId, PostingHit, PostingRangeRequest};

use crate::facade::FederationRouting;
use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use crate::plan::query::execute_plan_query;
use canbench_rs::bench;

const ORDERED_TOPK_ROWS: i64 = 512;
const ORDERED_TOPK_LIMIT: i64 = 10;
/// Residual predicate selectivity: every second row survives the pipeline filter.
const ORDERED_TOPK_FLAG_MODULUS: i64 = 2;

/// In-memory range postings in ascending encoded-key order — the real posting-walk
/// delivery contract (ADR 0081 §4) without storage cost inside the measurement.
struct BenchRangeIndex {
    range_hits: Vec<PostingHit>,
}

#[async_trait(?Send)]
impl PropertyIndexLookup for BenchRangeIndex {
    async fn lookup_equal(
        &self,
        _physical_index_id: PhysicalIndexId,
        _property_id: u32,
        _value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        Ok(Vec::new())
    }

    async fn lookup_range(
        &self,
        _physical_index_id: PhysicalIndexId,
        _property_id: u32,
        _req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        Ok(self.range_hits.clone())
    }

    async fn lookup_edge_range(
        &self,
        _physical_index_id: PhysicalIndexId,
        _property_id: u32,
        _req: &PostingRangeRequest,
        _label_id: Option<u16>,
    ) -> Result<Vec<gleaph_graph_kernel::index::EdgePostingHit>, PlanQueryError> {
        Ok(Vec::new())
    }

    async fn lookup_intersection(
        &self,
        _req: &gleaph_graph_kernel::index::IndexIntersectionRequest,
    ) -> Result<gleaph_graph_kernel::index::IndexIntersectionResult, PlanQueryError> {
        Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(Vec::new()))
    }

    fn local_shard_id(&self) -> ShardId {
        ShardId::new(0)
    }

    async fn posting_insert_at(
        &self,
        _shard_id: ShardId,
        _physical_index_id: PhysicalIndexId,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn posting_remove_at(
        &self,
        _shard_id: ShardId,
        _physical_index_id: PhysicalIndexId,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn label_posting_insert_at(
        &self,
        _shard_id: ShardId,
        _label_id: u32,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn label_posting_remove_at(
        &self,
        _shard_id: ShardId,
        _label_id: u32,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }
}

fn ordered_topk_query() -> &'static str {
    "MATCH (v:BenchOrdDel) WHERE v.score >= 0 AND v.flag = 0 \
     RETURN v.score AS score ORDER BY score LIMIT 10"
}

fn ordered_topk_stats() -> gleaph_gql_planner::stats::TableStats {
    let mut stats = gleaph_gql_planner::stats::TableStats::default();
    stats
        .label_cardinality
        .insert("BenchOrdDel".to_string(), ORDERED_TOPK_ROWS as u64);
    stats.range_indexed_vertex_properties.insert("score".into());
    stats
}

fn ordered_topk_plan() -> PhysicalPlan {
    let program = parser::parse(ordered_topk_query()).expect("parse");
    let block = program
        .transaction_activity
        .expect("tx activity")
        .body
        .expect("block");
    let gleaph_gql::ast::Statement::Query(composite) = &block.first else {
        panic!("expected query statement");
    };
    build_plan_with_schema_and_options(
        &composite.left,
        PlanBuildOptions {
            stats: Some(&ordered_topk_stats()),
            path_extensions: &gleaph_gql_integration::path_extension::GLEAPH_PATH_EXTENSION_HANDLER,
        },
        &NoSchema,
    )
    .expect("plan should build")
}

fn fixture() -> (GraphStore, BenchRangeIndex) {
    let store = GraphStore::new();
    store
        .set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            shard_id: ShardId::new(0),
            vector_canister: None,
        }))
        .expect("set index routing");

    let mut hits: Vec<(Vec<u8>, PostingHit)> = Vec::new();
    for score in 0..ORDERED_TOPK_ROWS {
        let vid = store
            .insert_vertex_named(
                ["BenchOrdDel"],
                [
                    ("score", Value::Int64(score)),
                    ("flag", Value::Int64(score % ORDERED_TOPK_FLAG_MODULUS)),
                ],
            )
            .expect("insert vertex");
        let key = gleaph_gql::value_to_index_key_bytes(&Value::Int64(score))
            .expect("encodable score")
            .expect("score key bytes");
        hits.push((
            key,
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: u32::try_from(u64::from(vid)).expect("vertex id"),
            },
        ));
    }
    hits.sort_by(|left, right| left.0.cmp(&right.0));

    (
        store,
        BenchRangeIndex {
            range_hits: hits.into_iter().map(|(_, hit)| hit).collect(),
        },
    )
}

fn strip_ordered_intent(plan: &mut PhysicalPlan) {
    if let Some(gleaph_gql_planner::plan::PlanOp::IndexScan {
        ordered_by_sort, ..
    }) = plan.ops.first_mut()
    {
        *ordered_by_sort = None;
    } else {
        panic!("leading op must be the anchored IndexScan");
    }
}

fn execute_topk(store: &GraphStore, index: &BenchRangeIndex, plan: &PhysicalPlan) -> usize {
    let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[
        crate::test_labels::property_id_for_name("score"),
    ]);
    let parameters: BTreeMap<String, Value> = BTreeMap::new();
    let result = pollster::block_on(execute_plan_query(
        store,
        plan,
        &parameters,
        Some(index),
        GqlExecutionContext::default(),
    ))
    .expect("ordered topk execution");
    assert_eq!(
        result.rows.len(),
        ORDERED_TOPK_LIMIT as usize,
        "top-k must deliver exactly k rows"
    );
    match result.rows.first().and_then(|row| row.get("score")) {
        Some(Value::Int64(score)) => {
            assert_eq!(
                *score, 0,
                "ascending delivery must start at the smallest key"
            );
        }
        other => panic!("expected int score binding, got {other:?}"),
    }
    result.rows.len()
}

/// Index-ordered TopK: the leading scan carries the `ordered_by_sort` intent, so the
/// executor stops at the tie-group boundary instead of sorting all 256 survivors.
#[bench(raw)]
fn bench_graph_order_by_index_ordered_topk_residual_filter() -> canbench_rs::BenchResult {
    let (store, index) = fixture();
    let plan = ordered_topk_plan();
    if !matches!(
        plan.ops.first(),
        Some(gleaph_gql_planner::plan::PlanOp::IndexScan {
            ordered_by_sort: Some(_),
            ..
        })
    ) {
        panic!("eligible plan must carry ordered-delivery intent");
    }

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("order_by_index_ordered_topk");
        black_box(execute_topk(
            black_box(&store),
            black_box(&index),
            black_box(&plan),
        ))
    })
}

/// Control: identical plan with the intent stripped — the same TopK must full-sort
/// every survivor before truncating to k.
#[bench(raw)]
fn bench_graph_order_by_sort_remaining_topk_residual_filter() -> canbench_rs::BenchResult {
    let (store, index) = fixture();
    let mut plan = ordered_topk_plan();
    strip_ordered_intent(&mut plan);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("order_by_sort_remaining_topk");
        black_box(execute_topk(
            black_box(&store),
            black_box(&index),
            black_box(&plan),
        ))
    })
}
