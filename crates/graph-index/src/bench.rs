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

/// Number of vertices in the walk (first) intersection arm. The second arm holds the even ids, so
/// the intersection result is `INTERSECTION_ARM_LEN / 2`.
const INTERSECTION_ARM_LEN: u32 = 4096;
const INTERSECTION_WALK_PROPERTY: u32 = 1;
const INTERSECTION_SIEVE_PROPERTY: u32 = 2;

fn index_key(text: &str) -> Vec<u8> {
    value_to_index_key_bytes(&Value::Text(text.to_string()))
        .expect("index key")
        .expect("indexable")
}

/// Two overlapping equality arms on one shard: walk arm = `[0, INTERSECTION_ARM_LEN)`, sieve arm =
/// even ids in the same range. Mirrors the all-vertex intersection inputs.
fn setup_two_arm_store() -> (IndexStore, Vec<u8>, Vec<u8>) {
    let (store, _router, owner) = setup_index_store();
    let walk_value = index_key("walk");
    let sieve_value = index_key("sieve");
    for vid in 0..INTERSECTION_ARM_LEN {
        store
            .posting_insert(
                owner,
                ShardId::new(0),
                INTERSECTION_WALK_PROPERTY,
                walk_value.clone(),
                vid,
            )
            .expect("walk arm insert");
        if vid % 2 == 0 {
            store
                .posting_insert(
                    owner,
                    ShardId::new(0),
                    INTERSECTION_SIEVE_PROPERTY,
                    sieve_value.clone(),
                    vid,
                )
                .expect("sieve arm insert");
        }
    }
    (store, walk_value, sieve_value)
}

/// Server-side materializing intersection (one in-heap set per arm) over two vertex arms.
#[bench(raw)]
fn bench_lookup_intersection_two_arms() -> canbench_rs::BenchResult {
    let (store, walk_value, sieve_value) = setup_two_arm_store();
    let req = IndexIntersectionRequest {
        specs: vec![
            IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY, walk_value),
            IndexEqualSpec::vertex(INTERSECTION_SIEVE_PROPERTY, sieve_value),
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
    let (store, walk_value, _sieve_value) = setup_two_arm_store();
    let req = LookupEqualPageRequest {
        property_id: INTERSECTION_WALK_PROPERTY,
        value: walk_value,
        after: None,
        limit: INTERSECTION_ARM_LEN,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_equal_page_walk_arm");
        let page = store
            .lookup_equal_page(black_box(&req))
            .expect("lookup_equal_page");
        black_box(page);
    })
}

/// The per-page `contains` sieve applied to one full walk page against the second arm — the work
/// the streaming intersection does per page in place of materializing the sieve arm.
#[bench(raw)]
fn bench_filter_hits_by_equal_page() -> canbench_rs::BenchResult {
    let (store, _walk_value, sieve_value) = setup_two_arm_store();
    let hits: Vec<PostingHit> = (0..INTERSECTION_ARM_LEN)
        .map(|vid| PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: vid,
        })
        .collect();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("filter_hits_by_equal_page");
        let survivors = store
            .filter_hits_by_equal(INTERSECTION_SIEVE_PROPERTY, black_box(&sieve_value), hits)
            .expect("filter_hits_by_equal");
        black_box(survivors);
    })
}

/// One server-side `lookup_intersection_page` call (walk one full page + merge-join sieve in-heap) —
/// the single inter-canister message the streaming consumers now loop over per page, replacing the
/// previous `lookup_equal_page` + N `filter_hits_by_equal` round trips.
#[bench(raw)]
fn bench_lookup_intersection_page() -> canbench_rs::BenchResult {
    let (store, walk_value, sieve_value) = setup_two_arm_store();
    let req = LookupIntersectionPageRequest {
        specs: vec![
            IndexEqualSpec::vertex(INTERSECTION_WALK_PROPERTY, walk_value),
            IndexEqualSpec::vertex(INTERSECTION_SIEVE_PROPERTY, sieve_value),
        ],
        after: None,
        limit: INTERSECTION_ARM_LEN,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_intersection_page");
        let page = store
            .lookup_intersection_page(black_box(&req))
            .expect("lookup_intersection_page");
        black_box(page);
    })
}

const RANGE_BENCH_PROPERTY: u32 = 3;
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

/// Range postings with values `[0, RANGE_INTERSECTION_COUNT)` and an equality arm that keeps
/// every fourth vertex. Mirrors the mixed equality-plus-range query shape.
fn setup_range_intersection_store() -> (IndexStore, Principal, Vec<u8>) {
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
            store
                .posting_insert(
                    owner,
                    ShardId::new(0),
                    RANGE_INTERSECTION_EQ_PROPERTY,
                    eq_value.clone(),
                    vid,
                )
                .expect("equality posting insert");
        }
    }
    (store, owner, eq_value)
}

/// One server-side range-walk page plus equality sieve for a half-open numeric interval covering
/// the full range. Measures the combined `lookup_range_intersection_page` primitive.
#[bench(raw)]
fn bench_lookup_range_intersection_page_full_range() -> canbench_rs::BenchResult {
    let (store, _owner, eq_value) = setup_range_intersection_store();
    let (low, high) = numeric_range_bounds(0, gleaph_gql::ast::CmpOp::Ge);
    let req = LookupRangeIntersectionPageRequest {
        range_property_id: RANGE_INTERSECTION_PROPERTY,
        low,
        high,
        equal_spec: IndexEqualSpec::vertex(RANGE_INTERSECTION_EQ_PROPERTY, eq_value),
        after: None,
        limit: 4096,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_intersection_page_full_range");
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
    let (store, _owner, eq_value) = setup_range_intersection_store();
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
        equal_spec: IndexEqualSpec::vertex(RANGE_INTERSECTION_EQ_PROPERTY, eq_value),
        after: None,
        limit: 1024,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_intersection_page_sparse_survivor");
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page sparse");
        black_box(page);
    })
}

const RANGE_INTERSECTION_ADVERSARIAL_PROPERTY: u32 = 6;
const RANGE_INTERSECTION_ADVERSARIAL_EQ_PROPERTY: u32 = 7;
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
        equal_spec: IndexEqualSpec::vertex(RANGE_INTERSECTION_ADVERSARIAL_EQ_PROPERTY, eq_value),
        after: None,
        limit: 10,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lookup_range_intersection_page_adversarial_span");
        let page = store
            .lookup_range_intersection_page(black_box(&req))
            .expect("lookup_range_intersection_page adversarial");
        black_box(page);
    })
}
