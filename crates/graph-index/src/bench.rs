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
    LookupIntersectionPageRequest, PostingHit,
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
