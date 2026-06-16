use super::*;
use crate::init::IndexInitArgs;
use crate::state::IndexError;
use candid::Principal;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_ic::PrincipalValue;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingHit, IndexEqualSpec, IndexIntersectionResult, LabelLookupPageRequest,
    LabelPostingCursor, PostingHit, PostingRangeRequest,
};

fn index_key(value: gleaph_gql::Value) -> Vec<u8> {
    value_to_index_key_bytes(&value).unwrap().unwrap()
}

fn test_router() -> Principal {
    Principal::from_slice(&[9])
}

fn init_test_store(store: &IndexStore) -> Principal {
    let router = test_router();
    store.init_from_args(&IndexInitArgs {
        controllers: vec![],
        router_canister: router,
    });
    router
}

fn attach_shard_canister(
    store: &IndexStore,
    router: Principal,
    shard_id: ShardId,
    shard_canister: Principal,
) {
    store
        .admin_attach_shard_canister(router, shard_id, shard_canister)
        .expect("attach shard canister");
}

#[test]
fn count_postings_by_value_groups_across_shards() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_a = Principal::from_slice(&[1]);
    let shard_b = Principal::from_slice(&[2]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_a);
    attach_shard_canister(&store, router, ShardId::new(1), shard_b);

    let property_id = 42;
    let us = index_key(Value::Text("US".into()));
    let uk = index_key(Value::Text("UK".into()));
    for (shard, owner, vid) in [
        (ShardId::new(0), shard_a, 1),
        (ShardId::new(0), shard_a, 2),
        (ShardId::new(1), shard_b, 3),
        (ShardId::new(0), shard_a, 4),
    ] {
        store
            .posting_insert(owner, shard, property_id, us.clone(), vid)
            .expect("insert us");
    }
    store
        .posting_insert(shard_a, ShardId::new(0), property_id, uk.clone(), 5)
        .expect("insert uk");

    let counts = store.count_postings_by_value(property_id, 2, 100, None);
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].encoded_value, us);
    assert_eq!(counts[0].count, 4);

    let all = store.count_postings_by_value(property_id, 1, 100, None);
    assert_eq!(all.len(), 2);
}

#[test]
fn count_postings_by_value_respects_vertex_filter() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_a = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_a);

    let property_id = 42;
    let us = index_key(Value::Text("US".into()));
    let uk = index_key(Value::Text("UK".into()));
    store
        .posting_insert(shard_a, ShardId::new(0), property_id, us.clone(), 1)
        .expect("us");
    store
        .posting_insert(shard_a, ShardId::new(0), property_id, us.clone(), 2)
        .expect("us");
    store
        .posting_insert(shard_a, ShardId::new(0), property_id, uk.clone(), 3)
        .expect("uk");

    let mut filter = std::collections::HashSet::new();
    filter.insert(pack_posting_vertex(ShardId::new(0), 1));
    let counts = store.count_postings_by_value(property_id, 1, 100, Some(&filter));
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].encoded_value, us);
    assert_eq!(counts[0].count, 1);
}

#[test]
fn insert_and_lookup_equal() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    store
        .posting_insert(shard_principal, ShardId::new(0), 42, b"v".to_vec(), 100)
        .expect("insert");

    let hits = store.lookup_equal(42, b"v");
    assert_eq!(
        hits,
        vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 100
        }]
    );
}

#[test]
fn insert_and_lookup_equal_principal_value_index_key() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    let p = Principal::from_text("aaaaa-aa").expect("management id");
    let key = index_key(Value::from(PrincipalValue(p)));

    store
        .posting_insert(shard_principal, ShardId::new(0), 42, key.clone(), 100)
        .expect("insert");

    let hits = store.lookup_equal(42, &key);
    assert_eq!(
        hits,
        vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 100
        }]
    );
}

#[test]
fn lookup_range_ge_and_lt_use_encoded_lex_order() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for (vid, val) in [
        (100u32, vec![1u8]),
        (200u32, vec![2u8]),
        (300u32, vec![3u8]),
    ] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 42, val, vid)
            .expect("insert");
    }

    let mut ge2: Vec<u32> = store
        .lookup_range(42, &PostingRangeRequest::Ge(vec![2]))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    ge2.sort_unstable();
    assert_eq!(ge2, vec![200, 300]);

    let mut lt2: Vec<u32> = store
        .lookup_range(42, &PostingRangeRequest::Lt(vec![2]))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    lt2.sort_unstable();
    assert_eq!(lt2, vec![100]);
}

#[test]
fn lookup_range_respects_sortable_value_key_boundaries() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for (vid, value) in [
        (10u32, gleaph_gql::Value::Int64(-1)),
        (20u32, gleaph_gql::Value::Uint8(0)),
        (30u32, gleaph_gql::Value::Int16(5)),
        (40u32, gleaph_gql::Value::Uint64(9)),
    ] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 42, index_key(value), vid)
            .expect("insert");
    }

    let five = index_key(gleaph_gql::Value::Uint8(5));
    let mut ge5: Vec<u32> = store
        .lookup_range(42, &PostingRangeRequest::Ge(five.clone()))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    ge5.sort_unstable();
    assert_eq!(ge5, vec![30, 40]);

    let mut lt5: Vec<u32> = store
        .lookup_range(42, &PostingRangeRequest::Lt(five))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    lt5.sort_unstable();
    assert_eq!(lt5, vec![10, 20]);
}

#[test]
fn lookup_range_text_prefix_boundaries_are_exact() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for (vid, value) in [
        (1u32, gleaph_gql::Value::Text("a".into())),
        (2u32, gleaph_gql::Value::Text("a\0".into())),
        (3u32, gleaph_gql::Value::Text("aa".into())),
    ] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 77, index_key(value), vid)
            .expect("insert");
    }

    let a = index_key(gleaph_gql::Value::Text("a".into()));
    assert_eq!(store.lookup_equal(77, &a)[0].vertex_id, 1);

    let mut gt_a: Vec<u32> = store
        .lookup_range(77, &PostingRangeRequest::Gt(a))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    gt_a.sort_unstable();
    assert_eq!(gt_a, vec![2, 3]);
}

#[test]
fn lookup_range_respects_list_value_key_boundaries() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    let values = [
        (10u32, gleaph_gql::Value::List(vec![])),
        (
            20u32,
            gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(1)]),
        ),
        (
            30u32,
            gleaph_gql::Value::List(vec![
                gleaph_gql::Value::Int64(1),
                gleaph_gql::Value::Int64(2),
            ]),
        ),
        (
            40u32,
            gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(2)]),
        ),
    ];
    for (vid, value) in values {
        store
            .posting_insert(shard_principal, ShardId::new(0), 88, index_key(value), vid)
            .expect("insert");
    }

    let one = index_key(gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(1)]));
    let two = index_key(gleaph_gql::Value::List(vec![gleaph_gql::Value::Int64(2)]));

    let mut ge_one: Vec<u32> = store
        .lookup_range(88, &PostingRangeRequest::Ge(one))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    ge_one.sort_unstable();
    assert_eq!(ge_one, vec![20, 30, 40]);

    let mut lt_two: Vec<u32> = store
        .lookup_range(88, &PostingRangeRequest::Lt(two))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    lt_two.sort_unstable();
    assert_eq!(lt_two, vec![10, 20, 30]);
}

#[test]
fn lookup_range_respects_record_value_key_boundaries() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for (vid, value) in [
        (
            10u32,
            gleaph_gql::Value::Record(vec![("a".into(), gleaph_gql::Value::Int64(1))]),
        ),
        (
            20u32,
            gleaph_gql::Value::Record(vec![("a".into(), gleaph_gql::Value::Int64(2))]),
        ),
        (
            30u32,
            gleaph_gql::Value::Record(vec![("b".into(), gleaph_gql::Value::Int64(1))]),
        ),
    ] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 99, index_key(value), vid)
            .expect("insert");
    }

    let same_key = index_key(gleaph_gql::Value::Record(vec![
        ("b".into(), gleaph_gql::Value::Int64(2)),
        ("a".into(), gleaph_gql::Value::Int64(1)),
    ]));
    assert_eq!(
        same_key,
        index_key(gleaph_gql::Value::Record(vec![
            ("a".into(), gleaph_gql::Value::Int64(1)),
            ("b".into(), gleaph_gql::Value::Int64(2)),
        ]))
    );

    let bound = index_key(gleaph_gql::Value::Record(vec![(
        "a".into(),
        gleaph_gql::Value::Int64(2),
    )]));
    let mut ge_bound: Vec<u32> = store
        .lookup_range(99, &PostingRangeRequest::Ge(bound))
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    ge_bound.sort_unstable();
    assert_eq!(ge_bound, vec![20, 30]);
}

#[test]
fn admin_attach_shard_canister_idempotent_same_principal() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard = Principal::from_slice(&[2]);
    store
        .admin_attach_shard_canister(router, ShardId::new(1), shard)
        .expect("first register");
    store
        .admin_attach_shard_canister(router, ShardId::new(1), shard)
        .expect("idempotent re-register");
}

#[test]
fn admin_attach_shard_canister_rejects_principal_change() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let a = Principal::self_authenticating([1u8; 32]);
    let b = Principal::self_authenticating([2u8; 32]);
    store
        .admin_attach_shard_canister(router, ShardId::new(1), a)
        .unwrap();
    assert_eq!(
        store.admin_attach_shard_canister(router, ShardId::new(1), b),
        Err(IndexError::ShardCanisterAlreadyAttached)
    );
}

#[test]
fn admin_attach_shard_canister_rejects_anonymous_principal() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    assert_eq!(
        store.admin_attach_shard_canister(router, ShardId::new(3), Principal::anonymous()),
        Err(IndexError::InvalidPrincipalInRegistry)
    );
}

#[test]
fn admin_attach_shard_canister_rejects_non_router_caller() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let other = Principal::from_slice(&[8]);
    assert_eq!(
        store.admin_attach_shard_canister(other, ShardId::new(1), Principal::from_slice(&[1])),
        Err(IndexError::NotAuthorized)
    );
    let _ = router;
}

#[test]
fn lookup_intersection_returns_vertices_in_all_specs() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    store
        .posting_insert(shard_principal, ShardId::new(0), 1, b"alice".to_vec(), 10)
        .expect("uid alice v10");
    store
        .posting_insert(shard_principal, ShardId::new(0), 1, b"alice".to_vec(), 20)
        .expect("uid alice v20");
    store
        .posting_insert(shard_principal, ShardId::new(0), 2, b"a@b.c".to_vec(), 20)
        .expect("email v20");
    store
        .posting_insert(shard_principal, ShardId::new(0), 2, b"a@b.c".to_vec(), 30)
        .expect("email v30");

    let result = store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
        specs: vec![
            IndexEqualSpec::vertex(1, b"alice".to_vec()),
            IndexEqualSpec::vertex(2, b"a@b.c".to_vec()),
        ],
    });
    assert_eq!(
        result,
        IndexIntersectionResult::Vertices(vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 20
        }])
    );
}

#[test]
fn lookup_intersection_empty_when_disjoint() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    store
        .posting_insert(shard_principal, ShardId::new(0), 1, b"alice".to_vec(), 10)
        .expect("uid");
    store
        .posting_insert(shard_principal, ShardId::new(0), 2, b"bob".to_vec(), 20)
        .expect("email");

    let result = store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
        specs: vec![
            IndexEqualSpec::vertex(1, b"alice".to_vec()),
            IndexEqualSpec::vertex(2, b"bob".to_vec()),
        ],
    });
    assert_eq!(result, IndexIntersectionResult::Vertices(vec![]));
}

#[test]
fn lookup_intersection_requires_two_specs() {
    let store = IndexStore::new();
    let result = store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
        specs: vec![IndexEqualSpec::vertex(1, b"x".to_vec())],
    });
    assert_eq!(result, IndexIntersectionResult::Vertices(vec![]));
}

#[test]
fn lookup_intersection_mixed_vertex_and_edge_projects_owners() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let owner = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), owner);

    store
        .posting_insert(owner, ShardId::new(0), 10, b"30".to_vec(), 100)
        .expect("age");
    store
        .edge_posting_insert(owner, ShardId::new(0), 20, b"5".to_vec(), 7, 100, 2)
        .expect("weight edge");
    store
        .edge_posting_insert(owner, ShardId::new(0), 20, b"5".to_vec(), 7, 101, 0)
        .expect("other owner");

    let result = store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
        specs: vec![
            IndexEqualSpec::vertex(10, b"30".to_vec()),
            IndexEqualSpec::edge(20, b"5".to_vec(), Some(7)),
        ],
    });
    assert_eq!(
        result,
        IndexIntersectionResult::Vertices(vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 100,
        }])
    );
}

#[test]
fn lookup_intersection_all_edge_arms_returns_edge_hits() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let owner = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), owner);

    store
        .edge_posting_insert(owner, ShardId::new(0), 30, b"1".to_vec(), 9, 50, 1)
        .expect("prop a");
    store
        .edge_posting_insert(owner, ShardId::new(0), 31, b"2".to_vec(), 9, 50, 1)
        .expect("prop b");

    let result = store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
        specs: vec![
            IndexEqualSpec::edge(30, b"1".to_vec(), Some(9)),
            IndexEqualSpec::edge(31, b"2".to_vec(), Some(9)),
        ],
    });
    assert_eq!(
        result,
        IndexIntersectionResult::Edges(vec![EdgePostingHit {
            shard_id: ShardId::new(0),
            owner_vertex_id: 50,
            label_id: 9,
            slot_index: 1,
        }])
    );
}

#[test]
fn insert_and_lookup_label() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    store
        .label_posting_insert(shard_principal, ShardId::new(0), 3, 100)
        .expect("insert");
    store
        .label_posting_insert(shard_principal, ShardId::new(0), 3, 200)
        .expect("insert");
    store
        .label_posting_insert(shard_principal, ShardId::new(0), 4, 300)
        .expect("other label");

    let hits = store.lookup_label(3);
    assert_eq!(hits.len(), 2);
    assert!(hits.contains(&PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: 100
    }));
    assert!(hits.contains(&PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: 200
    }));
    assert!(store.lookup_label(4).contains(&PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: 300
    }));
}

#[test]
fn label_posting_remove() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    store
        .label_posting_insert(shard_principal, ShardId::new(0), 1, 10)
        .expect("insert");
    store
        .label_posting_remove(shard_principal, ShardId::new(0), 1, 10)
        .expect("remove");
    assert!(store.lookup_label(1).is_empty());
}

#[test]
fn filter_hits_by_label_keeps_members_only() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    store
        .label_posting_insert(shard_principal, ShardId::new(0), 2, 10)
        .expect("label");
    store
        .label_posting_insert(shard_principal, ShardId::new(0), 2, 30)
        .expect("label");

    let hits = vec![
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 10,
        },
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 20,
        },
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 30,
        },
    ];
    let filtered = store.filter_hits_by_label(2, &hits);
    assert_eq!(
        filtered,
        vec![
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 10
            },
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 30
            },
        ]
    );
}

#[test]
fn lookup_label_for_shard_returns_only_local_shard() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_a = Principal::from_slice(&[1]);
    let shard_b = Principal::from_slice(&[2]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_a);
    attach_shard_canister(&store, router, ShardId::new(1), shard_b);

    store
        .label_posting_insert(shard_a, ShardId::new(0), 3, 10)
        .expect("shard 7");
    store
        .label_posting_insert(shard_b, ShardId::new(1), 3, 20)
        .expect("shard 9");

    let hits = store.lookup_label_for_shard(3, ShardId::new(0));
    assert_eq!(
        hits,
        vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 10
        }]
    );
}

#[test]
fn lookup_label_page_paginates_within_shard() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_a = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_a);

    for vid in [1u32, 2, 3] {
        store
            .label_posting_insert(shard_a, ShardId::new(0), 4, vid)
            .expect("insert");
    }

    let page1 = store.lookup_label_page(&LabelLookupPageRequest {
        vertex_label_id: 4,
        shard_id: ShardId::new(0),
        after: None,
        limit: 2,
    });
    assert_eq!(page1.hits.len(), 2);
    assert!(!page1.done);
    assert_eq!(
        page1.next,
        Some(LabelPostingCursor {
            shard_id: ShardId::new(0),
            vertex_id: 2
        })
    );

    let page2 = store.lookup_label_page(&LabelLookupPageRequest {
        vertex_label_id: 4,
        shard_id: ShardId::new(0),
        after: page1.next,
        limit: 2,
    });
    assert_eq!(page2.hits.len(), 1);
    assert!(page2.done);
}

#[test]
fn lookup_label_intersection_returns_common_vertices() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for vid in [10u32, 20, 30] {
        store
            .label_posting_insert(shard_principal, ShardId::new(0), 1, vid)
            .expect("L1");
        store
            .label_posting_insert(shard_principal, ShardId::new(0), 2, vid)
            .expect("L2");
    }
    store
        .label_posting_insert(shard_principal, ShardId::new(0), 1, 40)
        .expect("L1 only");

    let hits = store.lookup_label_intersection(&[1, 2]);
    assert_eq!(hits.len(), 3);
    assert!(hits.contains(&PostingHit {
        shard_id: ShardId::new(0),
        vertex_id: 10
    }));
    assert!(!hits.iter().any(|hit| hit.vertex_id == 40));
}

#[test]
fn count_postings_by_value_for_label_sieves_by_label() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    let property_id = 42;
    let us = index_key(Value::Text("US".into()));
    let uk = index_key(Value::Text("UK".into()));
    for vid in [1, 2, 3] {
        store
            .posting_insert(
                shard_principal,
                ShardId::new(0),
                property_id,
                us.clone(),
                vid,
            )
            .expect("us");
        store
            .label_posting_insert(shard_principal, ShardId::new(0), 5, vid)
            .expect("person");
    }
    store
        .posting_insert(shard_principal, ShardId::new(0), property_id, uk.clone(), 4)
        .expect("uk unlabeled");

    let counts = store.count_postings_by_value_for_label(property_id, 5, 1, 100);
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].encoded_value, us);
    assert_eq!(counts[0].count, 3);
}

#[test]
fn edge_posting_insert_remove_and_lookup_equal() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let owner = Principal::from_slice(&[3]);
    attach_shard_canister(&store, router, ShardId::new(0), owner);

    let property_id = 77;
    let value = index_key(Value::Int64(5));
    store
        .edge_posting_insert(
            owner,
            ShardId::new(0),
            property_id,
            value.clone(),
            9,
            100,
            2,
        )
        .expect("insert");
    store
        .edge_posting_insert(
            owner,
            ShardId::new(0),
            property_id,
            value.clone(),
            9,
            101,
            0,
        )
        .expect("insert other slot");

    let hits = store.lookup_edge_equal(property_id, &value, Some(9));
    assert_eq!(hits.len(), 2);
    assert!(
        hits.iter()
            .any(|h| h.owner_vertex_id == 100 && h.slot_index == 2)
    );
    assert!(
        hits.iter()
            .any(|h| h.owner_vertex_id == 101 && h.slot_index == 0)
    );

    store
        .edge_posting_remove(
            owner,
            ShardId::new(0),
            property_id,
            value.clone(),
            9,
            100,
            2,
        )
        .expect("remove");
    let remaining = store.lookup_edge_equal(property_id, &value, None);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].owner_vertex_id, 101);
}

#[test]
fn edge_posting_lookup_filters_by_label_prefix() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let owner = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), owner);

    let property_id = 88;
    let value = index_key(Value::Int64(1));
    store
        .edge_posting_insert(owner, ShardId::new(0), property_id, value.clone(), 1, 10, 0)
        .expect("label 1");
    store
        .edge_posting_insert(owner, ShardId::new(0), property_id, value.clone(), 2, 11, 0)
        .expect("label 2");

    assert_eq!(
        store.lookup_edge_equal(property_id, &value, Some(1)).len(),
        1
    );
    assert_eq!(store.lookup_edge_equal(property_id, &value, None).len(), 2);
}
