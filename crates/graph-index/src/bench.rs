//! Phase 8 stable-memory layout benchmarks (ADR 0007 §6).
//!
//! Run from `crates/graph-index`: `canbench` (see `canbench.yml`).

use crate::IndexStore;
use crate::init::IndexInitArgs;
use canbench_rs::bench;
use candid::Principal;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    IndexEqualSpec, IndexIntersectionRequest, LookupEqualPageRequest,
    LookupIntersectionPageRequest, LookupRangeIntersectionPageRequest, LookupRangePageRequest,
    PostingHit, PostingRangeRequest,
};
use std::cell::Cell;
use std::hint::black_box;

thread_local! {
    static POSTING_BENCH_SEQ: Cell<u32> = const { Cell::new(0) };
}

fn setup_index_store() -> (IndexStore, Principal, Principal) {
    let store = IndexStore::new();
    let router = Principal::from_slice(&[9]);
    let owner = Principal::from_slice(&[1]);
    store
        .init_from_args(&IndexInitArgs {
            router_canister: router,
        })
        .expect("non-anonymous router init");
    store
        .admin_attach_shard_canister(router, GraphId::from_raw(1), 1, 0, ShardId::new(0), owner)
        .expect("attach shard canister");
    (store, router, owner)
}

fn posting_insert_round(store: &IndexStore, owner: Principal) {
    let seq = POSTING_BENCH_SEQ.with(|c| {
        let n = c.get();
        c.set(n.wrapping_add(1));
        n
    });
    let property_id = 7u32;
    let value = value_to_index_key_bytes(&Value::Text(format!("bench_{seq}")))
        .expect("index key")
        .expect("indexable");
    for vid in 0..64u32 {
        store
            .posting_insert(owner, ShardId::new(0), property_id, value.clone(), vid)
            .expect("posting insert");
    }
}

#[bench(raw)]
fn bench_layout_index_posting_insert_64() -> canbench_rs::BenchResult {
    let (store, _router, owner) = setup_index_store();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_index_posting_insert");
        posting_insert_round(black_box(&store), owner);
    })
}

/// Fixed-shape dense arm-count comparison. The walk arm and every sieve arm contain the same
/// 1024-vertex set (`vid % 8` in `{0, 1}`) within a 4096-id range, so the intersection is also
/// exactly those 1024 vertices. Each arm therefore has 1024 postings, the walk page is dense
/// (span 4096, length 1024, threshold 4096), and the per-sieve merge-join is dense. Only the
/// number of arms varies across the 2/4/8 series.
const INTERSECTION_WALK_RANGE: u32 = 4096;
const INTERSECTION_WALK_CARDINALITY: u32 = 1024;
const INTERSECTION_WALK_PROPERTY: u32 = 1;
const INTERSECTION_SIEVE_PROPERTY: u32 = 2;

fn in_walk_set(vid: u32) -> bool {
    vid.is_multiple_of(8) || vid % 8 == 1
}

fn index_key(text: &str) -> Vec<u8> {
    value_to_index_key_bytes(&Value::Text(text.to_string()))
        .expect("index key")
        .expect("indexable")
}

fn setup_n_arm_store(arm_count: u32) -> (IndexStore, Vec<Vec<u8>>) {
    assert!(
        (2..=8).contains(&arm_count),
        "benchmarked arm count must be within the supported 2..=8 range"
    );
    let (store, _router, owner) = setup_index_store();
    let values: Vec<Vec<u8>> = (0..arm_count)
        .map(|i| index_key(&format!("arm_{i}")))
        .collect();

    // Shared set of 1024 vertices that every arm contains. The intersection is exactly this set.
    let shared_set: std::collections::HashSet<u32> = (0..INTERSECTION_WALK_RANGE)
        .filter(|v| in_walk_set(*v))
        .collect();
    assert_eq!(
        shared_set.len(),
        INTERSECTION_WALK_CARDINALITY as usize,
        "shared set size must be 1024"
    );

    // Every arm (walk + sieves) contains the same shared set.
    for (arm, value) in values.iter().enumerate() {
        let property_id = INTERSECTION_WALK_PROPERTY + arm as u32;
        for vid in &shared_set {
            store
                .posting_insert(owner, ShardId::new(0), property_id, value.clone(), *vid)
                .expect("arm insert");
        }
    }

    // Sanity check: `lookup_intersection_page` over all arms returns exactly the shared set.
    let sanity_req = LookupIntersectionPageRequest {
        specs: (0..arm_count)
            .map(|i| {
                IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY + i, values[i as usize].clone())
            })
            .collect(),
        after: None,
        limit: INTERSECTION_WALK_RANGE,
    };
    let sanity = store
        .lookup_intersection_page(&sanity_req)
        .expect("dense fixture sanity check");
    let mut sanity_ids: Vec<u32> = sanity.hits.iter().map(|h| h.vertex_id).collect();
    sanity_ids.sort_unstable();
    let mut expected_ids: Vec<u32> = shared_set.iter().copied().collect();
    expected_ids.sort_unstable();
    assert_eq!(
        sanity_ids, expected_ids,
        "dense fixture intersection must equal the fixed shared set"
    );

    (store, values)
}
/// Server-side materializing intersection (one in-heap set per arm) over two vertex arms.
#[bench(raw)]
fn bench_lookup_intersection_two_arms() -> canbench_rs::BenchResult {
    let (store, values) = setup_n_arm_store(2);
    let req = IndexIntersectionRequest {
        specs: vec![
            IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY, values[0].clone()),
            IndexEqualSpec::vertex(INTERSECTION_SIEVE_PROPERTY, values[1].clone()),
        ],
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_intersection_two_arms");
        let result = store
            .lookup_intersection(black_box(&req))
            .expect("lookup_intersection");
        black_box(result);
    })
}

/// One streamed page of the walk arm (`lookup_equal_page`) — the bounded read the query consumers
/// loop over instead of collecting a full bucket.
#[bench(raw)]
fn bench_lookup_equal_page_walk_arm() -> canbench_rs::BenchResult {
    let (store, values) = setup_n_arm_store(2);
    let req = LookupEqualPageRequest {
        property_id: INTERSECTION_WALK_PROPERTY,
        value: values[0].clone(),
        after: None,
        limit: INTERSECTION_WALK_RANGE,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_equal_page_walk_arm");
        let page = store
            .lookup_equal_page(black_box(&req))
            .expect("lookup_equal_page");
        black_box(page);
    })
}

/// The per-page dense merge-join sieve applied to one full 1024-hit walk page against the second
/// arm — the work the streaming intersection does per page in place of materializing the sieve arm.
/// This uses the same `in_walk_set` 1024 hits as the paged intersection benchmarks so it is directly
/// comparable to a single sieve arm in the dense series.
#[bench(raw)]
fn bench_filter_hits_by_equal_page() -> canbench_rs::BenchResult {
    let (store, values) = setup_n_arm_store(2);
    let hits: Vec<PostingHit> = (0..INTERSECTION_WALK_RANGE)
        .filter(|v| in_walk_set(*v))
        .map(|vid| PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: vid,
        })
        .collect();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("filter_hits_by_equal_page");
        let survivors = store
            .filter_hits_by_equal(
                INTERSECTION_SIEVE_PROPERTY,
                black_box(&values[1]),
                hits.clone(),
            )
            .expect("filter_hits_by_equal");
        black_box(survivors);
    })
}

/// One server-side `lookup_intersection_page` call with two arms.
#[bench(raw)]
fn bench_lookup_intersection_page_two_arms() -> canbench_rs::BenchResult {
    let (store, values) = setup_n_arm_store(2);
    let req = LookupIntersectionPageRequest {
        specs: vec![
            IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY, values[0].clone()),
            IndexEqualSpec::vertex(INTERSECTION_SIEVE_PROPERTY, values[1].clone()),
        ],
        after: None,
        limit: INTERSECTION_WALK_RANGE,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_intersection_page_two_arms");
        let page = store
            .lookup_intersection_page(black_box(&req))
            .expect("lookup_intersection_page");
        black_box(page);
    })
}

/// One server-side `lookup_intersection_page` call with four dense arms.
#[bench(raw)]
fn bench_lookup_intersection_page_four_arms() -> canbench_rs::BenchResult {
    let (store, values) = setup_n_arm_store(4);
    let req = LookupIntersectionPageRequest {
        specs: (0..4)
            .map(|i| {
                IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY + i, values[i as usize].clone())
            })
            .collect(),
        after: None,
        limit: INTERSECTION_WALK_RANGE,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_intersection_page_four_arms");
        let page = store
            .lookup_intersection_page(black_box(&req))
            .expect("lookup_intersection_page");
        black_box(page);
    })
}

/// One server-side `lookup_intersection_page` call with eight dense arms.
#[bench(raw)]
fn bench_lookup_intersection_page_eight_arms() -> canbench_rs::BenchResult {
    let (store, values) = setup_n_arm_store(8);
    let req = LookupIntersectionPageRequest {
        specs: (0..8)
            .map(|i| {
                IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY + i, values[i as usize].clone())
            })
            .collect(),
        after: None,
        limit: INTERSECTION_WALK_RANGE,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_intersection_page_eight_arms");
        let page = store
            .lookup_intersection_page(black_box(&req))
            .expect("lookup_intersection_page");
        black_box(page);
    })
}

/// Sparse 8-way intersection: only one vertex survives across all arms and the walk-page hits are
/// scattered across the full index space, forcing the per-hit point-lookup path for every sieve
/// arm.
#[bench(raw)]
fn bench_lookup_intersection_page_eight_arms_scattered() -> canbench_rs::BenchResult {
    let (store, _router, owner) = setup_index_store();
    let arm_count = 8u32;
    let values: Vec<Vec<u8>> = (0..arm_count)
        .map(|i| index_key(&format!("arm_{i}")))
        .collect();

    // Walk arm: sparse, widely scattered hits so that `equal_sieve_dense_threshold_met` fails.
    // Choose vertices roughly 256 apart within [0, 2^24) to keep the walk page below the threshold.
    let walk_hits: Vec<u32> = (0..INTERSECTION_WALK_RANGE).map(|i| i * 256).collect();
    for vid in &walk_hits {
        store
            .posting_insert(
                owner,
                ShardId::new(0),
                INTERSECTION_WALK_PROPERTY,
                values[0].clone(),
                *vid,
            )
            .expect("scattered walk insert");
    }

    // Sieve arms: only the first walk vertex survives in every arm. All other walk hits are unique
    // to a single sieve arm (and never to the walk arm), so the intersection yields one hit.
    for (arm, value) in values.iter().enumerate().skip(1) {
        let property_id = INTERSECTION_WALK_PROPERTY + arm as u32;
        // Shared survivor.
        store
            .posting_insert(
                owner,
                ShardId::new(0),
                property_id,
                value.clone(),
                walk_hits[0],
            )
            .expect("scattered survivor insert");
        // Private hits: one arm owns every other walk hit, so most lookups miss.
        for (i, vid) in walk_hits.iter().enumerate().skip(1) {
            if (i - 1) % (arm_count as usize - 1) == arm - 1 {
                store
                    .posting_insert(owner, ShardId::new(0), property_id, value.clone(), *vid)
                    .expect("scattered private insert");
            }
        }
    }

    let req = LookupIntersectionPageRequest {
        specs: (0..8)
            .map(|i| {
                IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY + i, values[i as usize].clone())
            })
            .collect(),
        after: None,
        limit: INTERSECTION_WALK_RANGE,
    };

    // Sanity-check outside the measured closure: exactly one vertex survives all eight arms.
    let sanity = store
        .lookup_intersection_page(&req)
        .expect("scattered sanity check");
    assert_eq!(
        sanity.hits,
        vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: walk_hits[0],
        }],
        "scattered fixture must leave exactly the first walk hit as the survivor"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_intersection_page_eight_arms_scattered");
        let page = store
            .lookup_intersection_page(black_box(&req))
            .expect("lookup_intersection_page");
        black_box(page);
    })
}

const RANGE_BENCH_PROPERTY: u32 = 20;
const RANGE_BENCH_COUNT: u32 = 4096;

fn setup_numeric_range_store() -> (IndexStore, Principal) {
    let (store, _router, owner) = setup_index_store();
    for vid in 0..RANGE_BENCH_COUNT {
        let value = value_to_index_key_bytes(&gleaph_gql::Value::Int64(vid as i64))
            .expect("index key")
            .expect("indexable");
        store
            .posting_insert(owner, ShardId::new(0), RANGE_BENCH_PROPERTY, value, vid)
            .expect("numeric posting insert");
    }
    // Add one later non-numeric posting to exercise encoded-domain isolation.
    let text_value = value_to_index_key_bytes(&gleaph_gql::Value::Text("zzzz".to_string()))
        .expect("text index key")
        .expect("indexable");
    store
        .posting_insert(
            owner,
            ShardId::new(0),
            RANGE_BENCH_PROPERTY,
            text_value,
            RANGE_BENCH_COUNT,
        )
        .expect("text posting insert");
    (store, owner)
}

fn numeric_range_bounds(value: i64, op: gleaph_gql::ast::CmpOp) -> (Vec<u8>, Vec<u8>) {
    gleaph_gql::numeric_range_bounds(&gleaph_gql::Value::Int64(value), op).expect("range bounds")
}

/// First page of a bounded numeric range that covers roughly half the postings.
#[bench(raw)]
fn bench_lookup_range_page_between_first_page() -> canbench_rs::BenchResult {
    let (store, _owner) = setup_numeric_range_store();
    let (low, high) = numeric_range_bounds(1024, gleaph_gql::ast::CmpOp::Ge);
    let req = LookupRangePageRequest {
        property_id: RANGE_BENCH_PROPERTY,
        range: PostingRangeRequest::Between { low, high },
        after: None,
        limit: 1024,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_page_between_first_page");
        let page = store
            .lookup_range_page(black_box(&req))
            .expect("lookup_range_page between");
        black_box(page);
    })
}

/// Resumed page after the first 1024 hits; exercises cursor continuation.
#[bench(raw)]
fn bench_lookup_range_page_between_resumed_page() -> canbench_rs::BenchResult {
    let (store, _owner) = setup_numeric_range_store();
    let (low, high) = numeric_range_bounds(1024, gleaph_gql::ast::CmpOp::Ge);
    let first = store
        .lookup_range_page(&LookupRangePageRequest {
            property_id: RANGE_BENCH_PROPERTY,
            range: PostingRangeRequest::Between {
                low: low.clone(),
                high: high.clone(),
            },
            after: None,
            limit: 1024,
        })
        .expect("first page");
    let after = first.next.expect("first page cursor");
    let req = LookupRangePageRequest {
        property_id: RANGE_BENCH_PROPERTY,
        range: PostingRangeRequest::Between { low, high },
        after: Some(after),
        limit: 1024,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_page_between_resumed_page");
        let page = store
            .lookup_range_page(black_box(&req))
            .expect("lookup_range_page resumed");
        black_box(page);
    })
}

/// Sparse range containing exactly one posting; measures scan-to-first-hit overhead
/// and encoded-domain boundary handling.
#[bench(raw)]
fn bench_lookup_range_page_between_sparse_range() -> canbench_rs::BenchResult {
    let (store, _owner) = setup_numeric_range_store();
    let low = value_to_index_key_bytes(&gleaph_gql::Value::Int64(2345))
        .expect("low key")
        .expect("indexable");
    let high = value_to_index_key_bytes(&gleaph_gql::Value::Int64(2346))
        .expect("high key")
        .expect("indexable");
    let req = LookupRangePageRequest {
        property_id: RANGE_BENCH_PROPERTY,
        range: PostingRangeRequest::Between { low, high },
        after: None,
        limit: 1024,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_page_between_sparse_range");
        let page = store
            .lookup_range_page(black_box(&req))
            .expect("lookup_range_page sparse");
        black_box(page);
    })
}

/// Full numeric comparison-domain range that must stop before later non-numeric postings.
#[bench(raw)]
fn bench_lookup_range_page_between_numeric_domain_boundary() -> canbench_rs::BenchResult {
    let (store, _owner) = setup_numeric_range_store();
    let (low, high) = numeric_range_bounds(0, gleaph_gql::ast::CmpOp::Ge);
    let req = LookupRangePageRequest {
        property_id: RANGE_BENCH_PROPERTY,
        range: PostingRangeRequest::Between { low, high },
        after: None,
        limit: 4096,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_page_between_numeric_domain_boundary");
        let page = store
            .lookup_range_page(black_box(&req))
            .expect("lookup_range_page numeric domain");
        black_box(page);
    })
}

const RANGE_INTERSECTION_PROPERTY: u32 = 4;
const RANGE_INTERSECTION_EQ_PROPERTY: u32 = 5;
const RANGE_INTERSECTION_COUNT: u32 = 4096;

/// Range postings with values `[0, RANGE_INTERSECTION_COUNT)` and a set of equality arms that keep
/// every fourth vertex, with one extra copy per additional sieve arm so that extra sieves still
/// see matching postings. Mirrors the mixed equality-plus-range query shape for 1, 4 and 8 arms.
fn setup_range_intersection_store(sieve_count: usize) -> (IndexStore, Principal, Vec<u8>) {
    assert!((1..=8).contains(&sieve_count));
    let (store, _router, owner) = setup_index_store();
    let eq_value = index_key("keep");
    for vid in 0..RANGE_INTERSECTION_COUNT {
        let price = value_to_index_key_bytes(&gleaph_gql::Value::Int64(vid as i64))
            .expect("index key")
            .expect("indexable");
        store
            .posting_insert(
                owner,
                ShardId::new(0),
                RANGE_INTERSECTION_PROPERTY,
                price,
                vid,
            )
            .expect("range posting insert");
        if vid % 4 == 0 {
            // Each sieve arm uses a distinct property id so we need one posting per arm for every
            // retained vertex.
            for arm in 0..sieve_count {
                store
                    .posting_insert(
                        owner,
                        ShardId::new(0),
                        RANGE_INTERSECTION_EQ_PROPERTY + arm as u32,
                        eq_value.clone(),
                        vid,
                    )
                    .expect("equality posting insert");
            }
        }
    }
    // Sanity check: the full range should return exactly the vertices retained by every sieve.
    let expected_hits = (0..RANGE_INTERSECTION_COUNT)
        .filter(|vid| vid % 4 == 0)
        .count();
    assert_eq!(
        expected_hits, 1024,
        "range-intersection fixture membership mismatch"
    );
    (store, owner, eq_value)
}

/// One server-side range-walk page plus one equality sieve for a half-open numeric interval covering
/// the full range. Measures the combined `lookup_range_intersection_page` primitive.
#[bench(raw)]
fn bench_lookup_range_intersection_page_full_range_one_sieve() -> canbench_rs::BenchResult {
    let (store, _owner, eq_value) = setup_range_intersection_store(1);
    let (low, high) = numeric_range_bounds(0, gleaph_gql::ast::CmpOp::Ge);
    let req = LookupRangeIntersectionPageRequest {
        range_property_id: RANGE_INTERSECTION_PROPERTY,
        low,
        high,
        equal_specs: vec![IndexEqualSpec::vertex(
            RANGE_INTERSECTION_EQ_PROPERTY,
            eq_value,
        )],
        after: None,
        limit: 4096,
    };
    // Membership sanity check outside measurement.
    let sanity = store
        .lookup_range_intersection_page(&req)
        .expect("one-sieve sanity lookup");
    assert_eq!(
        sanity.hits.len(),
        1024,
        "one-sieve sanity membership mismatch"
    );
    assert!(sanity.done, "one-sieve sanity should finish in one page");

    canbench_rs::bench_fn(|| {
        let _scope =
            canbench_rs::bench_scope("lookup_range_intersection_page_full_range_one_sieve");
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page");
        black_box(page);
    })
}

/// One server-side range-walk page plus four equality sieves for the full numeric range.
#[bench(raw)]
fn bench_lookup_range_intersection_page_full_range_four_sieves() -> canbench_rs::BenchResult {
    let (store, _owner, eq_value) = setup_range_intersection_store(4);
    let (low, high) = numeric_range_bounds(0, gleaph_gql::ast::CmpOp::Ge);
    let equal_specs = (0..4)
        .map(|i| {
            IndexEqualSpec::vertex(RANGE_INTERSECTION_EQ_PROPERTY + i as u32, eq_value.clone())
        })
        .collect();
    let req = LookupRangeIntersectionPageRequest {
        range_property_id: RANGE_INTERSECTION_PROPERTY,
        low,
        high,
        equal_specs,
        after: None,
        limit: 4096,
    };
    let sanity = store
        .lookup_range_intersection_page(&req)
        .expect("four-sieve sanity lookup");
    assert_eq!(
        sanity.hits.len(),
        1024,
        "four-sieve sanity membership mismatch"
    );
    assert!(sanity.done, "four-sieve sanity should finish in one page");

    canbench_rs::bench_fn(|| {
        let _scope =
            canbench_rs::bench_scope("lookup_range_intersection_page_full_range_four_sieves");
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page");
        black_box(page);
    })
}

/// One server-side range-walk page plus the maximum eight equality sieves for the full numeric range.
#[bench(raw)]
fn bench_lookup_range_intersection_page_full_range_eight_sieves() -> canbench_rs::BenchResult {
    let (store, _owner, eq_value) = setup_range_intersection_store(8);
    let (low, high) = numeric_range_bounds(0, gleaph_gql::ast::CmpOp::Ge);
    let equal_specs = (0..8)
        .map(|i| {
            IndexEqualSpec::vertex(RANGE_INTERSECTION_EQ_PROPERTY + i as u32, eq_value.clone())
        })
        .collect();
    let req = LookupRangeIntersectionPageRequest {
        range_property_id: RANGE_INTERSECTION_PROPERTY,
        low,
        high,
        equal_specs,
        after: None,
        limit: 4096,
    };
    let sanity = store
        .lookup_range_intersection_page(&req)
        .expect("eight-sieve sanity lookup");
    assert_eq!(
        sanity.hits.len(),
        1024,
        "eight-sieve sanity membership mismatch"
    );
    assert!(sanity.done, "eight-sieve sanity should finish in one page");

    canbench_rs::bench_fn(|| {
        let _scope =
            canbench_rs::bench_scope("lookup_range_intersection_page_full_range_eight_sieves");
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page");
        black_box(page);
    })
}

/// Sparse mixed range-equality request where the range contains exactly one hit and the equality
/// arm keeps it. Measures scan-to-first-survivor overhead.
#[bench(raw)]
fn bench_lookup_range_intersection_page_sparse_survivor() -> canbench_rs::BenchResult {
    let (store, _owner, eq_value) = setup_range_intersection_store(1);
    let low = value_to_index_key_bytes(&gleaph_gql::Value::Int64(1024))
        .expect("low key")
        .expect("indexable");
    let high = value_to_index_key_bytes(&gleaph_gql::Value::Int64(1025))
        .expect("high key")
        .expect("indexable");
    let req = LookupRangeIntersectionPageRequest {
        range_property_id: RANGE_INTERSECTION_PROPERTY,
        low,
        high,
        equal_specs: vec![IndexEqualSpec::vertex(
            RANGE_INTERSECTION_EQ_PROPERTY,
            eq_value,
        )],
        after: None,
        limit: 1024,
    };
    let sanity = store
        .lookup_range_intersection_page(&req)
        .expect("sparse sanity lookup");
    assert_eq!(sanity.hits.len(), 1, "sparse sanity membership mismatch");
    assert_eq!(
        sanity.hits[0],
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 1024,
        },
        "sparse sanity survivor mismatch"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_intersection_page_sparse_survivor");
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page sparse");
        black_box(page);
    })
}

/// Scattered range request with multiple far-apart hits across a 4M vertex span, filtered by
/// four equality sieves. The sparse range postings force the span-aware point-lookup path; the
/// equality bucket is densely populated between hits so an unbounded scan would sweep many
/// postings. Membership is verified once before measurement so assertions do not pollute the
/// persisted instruction counts.
#[bench(raw)]
fn bench_lookup_range_intersection_page_scattered_survivor_four_sieves() -> canbench_rs::BenchResult
{
    let (store, _owner, eq_value) = setup_range_intersection_scattered_store(4);
    let (low, high) = numeric_range_bounds(-1, gleaph_gql::ast::CmpOp::Ge);
    let equal_specs = (0..4)
        .map(|i| {
            IndexEqualSpec::vertex(
                RANGE_INTERSECTION_SCATTERED_EQ_PROPERTY + i as u32,
                eq_value.clone(),
            )
        })
        .collect();
    let req = LookupRangeIntersectionPageRequest {
        range_property_id: RANGE_INTERSECTION_SCATTERED_PROPERTY,
        low,
        high,
        equal_specs,
        after: None,
        limit: 10,
    };

    // Sanity check outside the measured closure: assert the expected branch and full survivor set.
    let sanity = store
        .lookup_range_intersection_page(&req)
        .expect("scattered sanity lookup");
    let expected: Vec<u32> = SCATTERED_SURVIVORS.to_vec();
    assert!(
        sanity.done,
        "scattered sanity page should be terminal within limit"
    );
    assert_eq!(
        sanity.hits.iter().map(|h| h.vertex_id).collect::<Vec<_>>(),
        expected,
        "scattered sanity membership mismatch"
    );
    assert_eq!(
        sanity.hits.len(),
        expected.len(),
        "scattered sanity hit count mismatch"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(
            "lookup_range_intersection_page_scattered_survivor_four_sieves",
        );
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page scattered");
        black_box(page);
    })
}

const RANGE_INTERSECTION_SCATTERED_PROPERTY: u32 = 6;
const RANGE_INTERSECTION_SCATTERED_EQ_PROPERTY: u32 = 7;
const SCATTERED_SPAN: u32 = 4_000_000;
const SCATTERED_BUCKET_DENSITY: u32 = 100_000;
const SCATTERED_SURVIVORS: [u32; 4] = [0, 1_000_000, 2_000_000, 3_000_000];

/// Sparse range postings at fixed far-apart positions, plus a dense equality bucket between them
/// that shares no vertex ids with the range hits (so only point-lookups of the range hits can
/// succeed). Each sieve arm gets a matching equality posting for every survivor. The range itself is
/// encoded using the same numeric key encoding as the other fixtures.
fn setup_range_intersection_scattered_store(
    sieve_count: usize,
) -> (IndexStore, Principal, Vec<u8>) {
    assert!((1..=8).contains(&sieve_count));
    let (store, _router, owner) = setup_index_store();
    let eq_value = index_key("keep");

    // Insert far-apart range postings. Values are sparse enough that a naive dense scan over the
    // equality bucket between them would be dominated by unrelated postings.
    for vid in SCATTERED_SURVIVORS {
        let price = value_to_index_key_bytes(&gleaph_gql::Value::Int64(vid as i64))
            .expect("index key")
            .expect("indexable");
        store
            .posting_insert(
                owner,
                ShardId::new(0),
                RANGE_INTERSECTION_SCATTERED_PROPERTY,
                price,
                vid,
            )
            .expect("scattered range posting insert");
    }

    // Dense equality bucket between survivors; none of these vids have a range posting.
    for i in 1..=SCATTERED_BUCKET_DENSITY {
        let vid =
            (i as u64 * (SCATTERED_SPAN as u64 - 1) / (SCATTERED_BUCKET_DENSITY as u64 + 1)) as u32;
        for arm in 0..sieve_count {
            store
                .posting_insert(
                    owner,
                    ShardId::new(0),
                    RANGE_INTERSECTION_SCATTERED_EQ_PROPERTY + arm as u32,
                    eq_value.clone(),
                    vid,
                )
                .expect("scattered equality bucket insert");
        }
    }

    // Each sieve arm gets a matching equality posting for every survivor.
    for vid in SCATTERED_SURVIVORS {
        for arm in 0..sieve_count {
            store
                .posting_insert(
                    owner,
                    ShardId::new(0),
                    RANGE_INTERSECTION_SCATTERED_EQ_PROPERTY + arm as u32,
                    eq_value.clone(),
                    vid,
                )
                .expect("scattered survivor equality insert");
        }
    }

    (store, owner, eq_value)
}

const RANGE_INTERSECTION_ADVERSARIAL_PROPERTY: u32 = 14;
const RANGE_INTERSECTION_ADVERSARIAL_EQ_PROPERTY: u32 = 15;
const ADVERSARIAL_SPAN: u32 = 4_000_000;
const ADVERSARIAL_BUCKET_DENSITY: u32 = 100_000;

/// Two range hits 4M vertices apart, with the  equality bucket densely populated between them
/// but no corresponding range postings for those intermediate vertices. An unbounded dense merge
/// scan would sweep 100k equality postings; the span-aware sieve must fall back to point lookups and
/// stay bounded by the page size.
fn setup_range_intersection_adversarial_store() -> (IndexStore, Principal, Vec<u8>) {
    let (store, _router, owner) = setup_index_store();
    let eq_value = index_key("keep");
    for vid in [0u32, ADVERSARIAL_SPAN] {
        let price = value_to_index_key_bytes(&gleaph_gql::Value::Int64(vid as i64))
            .expect("index key")
            .expect("indexable");
        store
            .posting_insert(
                owner,
                ShardId::new(0),
                RANGE_INTERSECTION_ADVERSARIAL_PROPERTY,
                price,
                vid,
            )
            .expect("adversarial range posting insert");
    }
    // Fill the  equality bucket densely between the two range hits, but without range
    // postings, so only the two actual range hits matter. A dense scan over this bucket would be
    // proportional to the bucket size, not to the page size.
    for i in 1..=ADVERSARIAL_BUCKET_DENSITY {
        let vid =
            ((ADVERSARIAL_SPAN as u64 * i as u64) / (ADVERSARIAL_BUCKET_DENSITY as u64 + 1)) as u32;
        store
            .posting_insert(
                owner,
                ShardId::new(0),
                RANGE_INTERSECTION_ADVERSARIAL_EQ_PROPERTY,
                eq_value.clone(),
                vid,
            )
            .expect("adversarial equality bucket insert");
    }
    // One matching equality arm for the first range hit.
    store
        .posting_insert(
            owner,
            ShardId::new(0),
            RANGE_INTERSECTION_ADVERSARIAL_EQ_PROPERTY,
            eq_value.clone(),
            0,
        )
        .expect("adversarial matching equality insert");
    (store, owner, eq_value)
}

/// Two range hits 4M vertices apart with one unrelated equality posting in between. The sieve work
/// must remain proportional to the page size, not to the vertex_id span.
#[bench(raw)]
fn bench_lookup_range_intersection_page_adversarial_span() -> canbench_rs::BenchResult {
    let (store, _owner, eq_value) = setup_range_intersection_adversarial_store();
    let (low, high) = numeric_range_bounds(-1, gleaph_gql::ast::CmpOp::Ge);
    let req = LookupRangeIntersectionPageRequest {
        range_property_id: RANGE_INTERSECTION_ADVERSARIAL_PROPERTY,
        low,
        high,
        equal_specs: vec![IndexEqualSpec::vertex(
            RANGE_INTERSECTION_ADVERSARIAL_EQ_PROPERTY,
            eq_value,
        )],
        after: None,
        limit: 10,
    };
    let sanity = store
        .lookup_range_intersection_page(&req)
        .expect("adversarial sanity lookup");
    assert!(
        sanity.done,
        "adversarial sanity should be terminal within limit"
    );
    assert_eq!(
        sanity.hits.iter().map(|h| h.vertex_id).collect::<Vec<_>>(),
        vec![0u32],
        "adversarial sanity membership mismatch"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_intersection_page_adversarial_span");
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page adversarial");
        black_box(page);
    })
}
