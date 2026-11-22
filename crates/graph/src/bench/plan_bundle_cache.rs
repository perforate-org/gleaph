//! Decoded plan-bundle cache hit cost (`crate::index::plan_cache`).
//!
//! Bulk seeding executes one plan template many times, so the hit path runs once per operation.
//! The bench fills the cache to its entry limit with same-length blobs that differ only in a
//! label, the case where key comparison has to read deepest into the blob, then looks up one of
//! them 256 times.

use canbench_rs::bench;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp};
use gleaph_gql_planner::wire::encode_block_plans;
use std::hint::black_box;

use crate::index::plan_cache::{
    PLAN_BUNDLE_CACHE_LIMITS, clear_plan_bundle_cache, decode_plan_bundle_cached,
};

fn insert_vertex_blob(index: usize) -> Vec<u8> {
    let plan = PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
        variable: Some("n".into()),
        labels: vec![format!("BenchSeedLabel{index:04}").as_str().into()],
        properties: vec![],
    }]);
    encode_block_plans(&[plan], true).expect("encode bench plan")
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_plan_bundle_cache_hit_256_full_cache() -> canbench_rs::BenchResult {
    clear_plan_bundle_cache();
    let blobs: Vec<Vec<u8>> = (0..PLAN_BUNDLE_CACHE_LIMITS.max_entries())
        .map(insert_vertex_blob)
        .collect();
    for blob in &blobs {
        decode_plan_bundle_cached(blob).expect("warm cache");
    }
    let hot = &blobs[blobs.len() / 2];
    canbench_rs::bench_fn(|| {
        for _ in 0..256 {
            black_box(decode_plan_bundle_cached(black_box(hot)).expect("cache hit"));
        }
    })
}
