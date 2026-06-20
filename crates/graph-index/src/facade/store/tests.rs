use super::*;
use crate::facade::stable::{INDEX_EDGE_POSTINGS, INDEX_VERTEX_POSTINGS};
use crate::init::IndexInitArgs;
use crate::state::IndexError;
use candid::Principal;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_ic::PrincipalValue;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{IndexPurgeKind, ShardDetachStepResult, ShardId};
use gleaph_graph_kernel::index::{
    EdgePostingCursor, EdgePostingHit, IndexEqualSpec, IndexIntersectionResult,
    LabelLookupPageRequest, LabelPostingCursor, LookupEdgeEqualPageRequest, LookupEqualPageRequest,
    LookupIntersectionPageRequest, LookupRangePageRequest, PostingHit, PostingRangeRequest,
    PropertyPostingCursor,
};

fn index_key(value: gleaph_gql::Value) -> Vec<u8> {
    value_to_index_key_bytes(&value).unwrap().unwrap()
}

fn test_router() -> Principal {
    Principal::from_slice(&[9])
}

fn init_test_store(store: &IndexStore) -> Principal {
    let router = test_router();
    store
        .init_from_args(&IndexInitArgs {
            router_canister: router,
        })
        .expect("non-anonymous router init");
    router
}

fn attach_shard_canister(
    store: &IndexStore,
    router: Principal,
    shard_id: ShardId,
    shard_canister: Principal,
) {
    const INDEX_GROUP_SIZE: u32 = 2;
    let group_index = shard_id.raw() / INDEX_GROUP_SIZE;
    store
        .admin_attach_shard_canister(
            router,
            GraphId::from_raw(1),
            INDEX_GROUP_SIZE,
            group_index,
            shard_id,
            shard_canister,
        )
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

    let hits = store.lookup_equal(42, b"v").expect("lookup_equal");
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

    let hits = store.lookup_equal(42, &key).expect("lookup_equal");
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
        .expect("lookup_range")
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    ge2.sort_unstable();
    assert_eq!(ge2, vec![200, 300]);

    let mut lt2: Vec<u32> = store
        .lookup_range(42, &PostingRangeRequest::Lt(vec![2]))
        .expect("lookup_range")
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
        .expect("lookup_range")
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    ge5.sort_unstable();
    assert_eq!(ge5, vec![30, 40]);

    let mut lt5: Vec<u32> = store
        .lookup_range(42, &PostingRangeRequest::Lt(five))
        .expect("lookup_range")
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
    assert_eq!(
        store.lookup_equal(77, &a).expect("lookup_equal")[0].vertex_id,
        1
    );

    let mut gt_a: Vec<u32> = store
        .lookup_range(77, &PostingRangeRequest::Gt(a))
        .expect("lookup_range")
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
        .expect("lookup_range")
        .into_iter()
        .map(|h| h.vertex_id)
        .collect();
    ge_one.sort_unstable();
    assert_eq!(ge_one, vec![20, 30, 40]);

    let mut lt_two: Vec<u32> = store
        .lookup_range(88, &PostingRangeRequest::Lt(two))
        .expect("lookup_range")
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
        .expect("lookup_range")
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
        .admin_attach_shard_canister(router, GraphId::from_raw(1), 1, 1, ShardId::new(1), shard)
        .expect("first register");
    store
        .admin_attach_shard_canister(router, GraphId::from_raw(1), 1, 1, ShardId::new(1), shard)
        .expect("idempotent re-register");
}

#[test]
fn admin_attach_shard_canister_rejects_principal_change() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let a = Principal::self_authenticating([1u8; 32]);
    let b = Principal::self_authenticating([2u8; 32]);
    store
        .admin_attach_shard_canister(router, GraphId::from_raw(1), 1, 1, ShardId::new(1), a)
        .unwrap();
    assert_eq!(
        store.admin_attach_shard_canister(router, GraphId::from_raw(1), 1, 1, ShardId::new(1), b),
        Err(IndexError::ShardCanisterAlreadyAttached)
    );
}

#[test]
fn admin_attach_shard_canister_rejects_anonymous_principal() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    assert_eq!(
        store.admin_attach_shard_canister(
            router,
            GraphId::from_raw(1),
            1,
            3,
            ShardId::new(3),
            Principal::anonymous(),
        ),
        Err(IndexError::InvalidPrincipalInRegistry)
    );
}

#[test]
fn init_from_args_rejects_anonymous_router_without_clearing_state() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard = Principal::from_slice(&[2]);
    attach_shard_canister(&store, router, ShardId::new(0), shard);

    // Seed a posting so we can prove postings are not cleared by a rejected re-init.
    let property_id = 42;
    let value = index_key(Value::Text("US".into()));
    store
        .posting_insert(shard, ShardId::new(0), property_id, value.clone(), 100)
        .expect("seed posting");
    assert_eq!(
        store
            .lookup_equal(property_id, &value)
            .expect("lookup_equal"),
        vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 100
        }]
    );

    // A re-init with an anonymous router must be rejected before any state is cleared.
    assert_eq!(
        store.init_from_args(&IndexInitArgs {
            router_canister: Principal::anonymous(),
        }),
        Err(IndexError::AnonymousRouter)
    );

    // Postings, catalog, and router configuration remain intact: the seeded posting is still
    // queryable, the previously attached shard canister still authorizes, and the anonymous
    // principal was not persisted as the router.
    assert_eq!(
        store
            .lookup_equal(property_id, &value)
            .expect("lookup_equal"),
        vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 100
        }],
        "posting must survive a rejected init"
    );
    assert_eq!(store.assert_shard_canister(shard, ShardId::new(0)), Ok(()));
    assert_eq!(
        store.assert_router_caller(Principal::anonymous()),
        Err(IndexError::NotAuthorized)
    );
    assert_eq!(store.assert_router_caller(router), Ok(()));
}

#[test]
fn assert_router_caller_rejects_anonymous_even_if_configured() {
    let store = IndexStore::new();
    let _router = init_test_store(&store);
    assert_eq!(
        store.assert_router_caller(Principal::anonymous()),
        Err(IndexError::NotAuthorized)
    );
}

#[test]
fn admin_attach_shard_canister_rejects_anonymous_router_caller() {
    let store = IndexStore::new();
    let _router = init_test_store(&store);
    assert_eq!(
        store.admin_attach_shard_canister(
            Principal::anonymous(),
            GraphId::from_raw(1),
            1,
            0,
            ShardId::new(0),
            Principal::from_slice(&[2]),
        ),
        Err(IndexError::NotAuthorized)
    );
}

#[test]
fn admin_attach_shard_canister_rejects_non_router_caller() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let other = Principal::from_slice(&[8]);
    assert_eq!(
        store.admin_attach_shard_canister(
            other,
            GraphId::from_raw(1),
            1,
            1,
            ShardId::new(1),
            Principal::from_slice(&[1]),
        ),
        Err(IndexError::NotAuthorized)
    );
    let _ = router;
}

#[test]
fn admin_attach_shard_canister_rejects_graph_ownership_mismatch() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard = Principal::from_slice(&[2]);
    store
        .admin_attach_shard_canister(router, GraphId::from_raw(1), 1, 0, ShardId::new(0), shard)
        .expect("first register");
    assert_eq!(
        store.admin_attach_shard_canister(
            router,
            GraphId::from_raw(2),
            1,
            0,
            ShardId::new(0),
            shard
        ),
        Err(IndexError::GraphOwnershipMismatch)
    );
}

#[test]
fn admin_attach_shard_canister_rejects_shard_out_of_group_range() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard = Principal::from_slice(&[2]);
    assert_eq!(
        store.admin_attach_shard_canister(
            router,
            GraphId::from_raw(1),
            4,
            1,
            ShardId::new(2),
            shard
        ),
        Err(IndexError::ShardOutOfRangeForGroup)
    );
}

#[test]
fn admin_detach_shard_canister_purges_shard_postings() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let graph_id = GraphId::from_raw(1);
    let shard_a = Principal::from_slice(&[1]);
    store
        .admin_attach_shard_canister(router, graph_id, 1, 0, ShardId::new(0), shard_a)
        .expect("attach shard 0");

    store
        .posting_insert(shard_a, ShardId::new(0), 42, b"v".to_vec(), 10)
        .expect("insert shard0 vertex posting");
    store
        .label_posting_insert(shard_a, ShardId::new(0), 7, 10)
        .expect("insert shard0 label posting");
    store
        .edge_posting_insert(shard_a, ShardId::new(0), 88, b"e".to_vec(), 3, 10, 0)
        .expect("insert shard0 edge posting");

    drive_detach_to_completion(&store, router, ShardId::new(0));

    assert!(
        store
            .lookup_equal(42, b"v")
            .expect("lookup_equal")
            .is_empty()
    );
    assert!(store.lookup_label(7).is_empty());
    assert!(
        store
            .lookup_edge_equal(88, b"e", Some(3))
            .expect("lookup_edge_equal")
            .is_empty()
    );
}

/// Drives the bounded shard detach steps to completion using the production
/// budget, returning the total keys removed across steps.
fn drive_detach_to_completion(store: &IndexStore, router: Principal, shard_id: ShardId) -> u32 {
    let mut resume = None;
    let mut removed_total = 0u32;
    loop {
        let step: ShardDetachStepResult = store
            .admin_detach_shard_canister(router, shard_id, resume)
            .expect("detach step");
        removed_total += step.removed;
        match step.next {
            Some(cursor) => resume = Some(cursor),
            None => {
                assert!(step.done);
                return removed_total;
            }
        }
    }
}

#[test]
fn admin_detach_shard_canister_requires_router_caller() {
    let store = IndexStore::new();
    let _router = init_test_store(&store);
    let intruder = Principal::from_slice(&[0xAB]);
    assert_eq!(
        store
            .admin_detach_shard_canister(intruder, ShardId::new(0), None)
            .err(),
        Some(IndexError::NotAuthorized)
    );
}

#[test]
fn bounded_detach_resumes_and_only_purges_target_shard() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let graph_id = GraphId::from_raw(1);
    let shard0 = Principal::from_slice(&[1]);
    let shard1 = Principal::from_slice(&[2]);
    store
        .admin_attach_shard_canister(router, graph_id, 4, 0, ShardId::new(0), shard0)
        .expect("attach shard 0");
    store
        .admin_attach_shard_canister(router, graph_id, 4, 0, ShardId::new(1), shard1)
        .expect("attach shard 1");

    // Several vertex postings on each shard under the same property.
    for vid in 0..5u32 {
        store
            .posting_insert(shard0, ShardId::new(0), 42, b"v".to_vec(), vid)
            .expect("insert shard0 posting");
        store
            .posting_insert(shard1, ShardId::new(1), 42, b"v".to_vec(), vid)
            .expect("insert shard1 posting");
    }

    // Budget of 1 examined key per step forces resume across the scan.
    let mut resume = None;
    let mut steps = 0u32;
    let mut removed_total = 0u32;
    loop {
        let step = store.detach_shard_step_for_test(ShardId::new(0), resume, 1);
        steps += 1;
        removed_total += step.removed;
        assert!(step.examined <= 1);
        match step.next {
            Some(cursor) => resume = Some(cursor),
            None => break,
        }
        assert!(steps < 1000, "bounded detach did not converge");
    }

    assert_eq!(removed_total, 5, "all shard 0 postings purged");
    assert!(steps > 5, "scan was actually bounded across multiple steps");
    // Shard 1 postings survive the targeted detach.
    let survivors = store.lookup_equal(42, b"v").expect("lookup_equal");
    assert_eq!(survivors.len(), 5);
    assert!(survivors.iter().all(|hit| hit.shard_id == ShardId::new(1)));
}

#[test]
fn bounded_vertex_purge_removes_only_target_property() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard0 = Principal::from_slice(&[1]);
    let shard1 = Principal::from_slice(&[2]);
    attach_shard_canister(&store, router, ShardId::new(0), shard0);
    attach_shard_canister(&store, router, ShardId::new(1), shard1);

    // Postings on the dropped property (42) across shards plus a neighbour (43)
    // bracketing it on either side (41, 43) to prove the range is scoped.
    for vid in 0..4u32 {
        for pid in [41u32, 42, 43] {
            store
                .posting_insert(shard0, ShardId::new(0), pid, b"v".to_vec(), vid)
                .expect("insert shard0 posting");
            store
                .posting_insert(shard1, ShardId::new(1), pid, b"v".to_vec(), vid)
                .expect("insert shard1 posting");
        }
    }

    // Budget of 1 examined key per step forces resume across the scan.
    let mut resume = None;
    let mut steps = 0u32;
    let mut removed_total = 0u32;
    loop {
        let step =
            store.purge_property_postings_step_for_test(IndexPurgeKind::Vertex, 42, 0, resume, 1);
        steps += 1;
        removed_total += step.removed;
        assert!(step.examined <= 1);
        match step.next {
            Some(cursor) => resume = Some(cursor),
            None => break,
        }
        assert!(steps < 1000, "bounded purge did not converge");
    }

    assert_eq!(
        removed_total, 8,
        "all property-42 postings purged across shards"
    );
    // budget 1 over the 8-key property range ⇒ one key per step (bounded/resumed).
    assert!(steps >= 8, "scan was actually bounded across multiple steps");
    assert!(store.lookup_equal(42, b"v").expect("lookup 42").is_empty());
    assert_eq!(store.lookup_equal(41, b"v").expect("lookup 41").len(), 8);
    assert_eq!(store.lookup_equal(43, b"v").expect("lookup 43").len(), 8);
}

#[test]
fn bounded_edge_purge_removes_only_target_label() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard0 = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard0);

    // Two edge indexes share property 88 under different labels (3 and 7); a
    // third property (89) brackets the range. Dropping the (88, label 3) index
    // must purge only its postings.
    for owner in 0..4u32 {
        store
            .edge_posting_insert(shard0, ShardId::new(0), 88, b"e".to_vec(), 3, owner, 0)
            .expect("insert label 3");
        store
            .edge_posting_insert(shard0, ShardId::new(0), 88, b"e".to_vec(), 7, owner, 0)
            .expect("insert label 7");
        store
            .edge_posting_insert(shard0, ShardId::new(0), 89, b"e".to_vec(), 3, owner, 0)
            .expect("insert other property");
    }

    let mut resume = None;
    let mut removed_total = 0u32;
    let mut steps = 0u32;
    loop {
        let step =
            store.purge_property_postings_step_for_test(IndexPurgeKind::Edge, 88, 3, resume, 1);
        steps += 1;
        removed_total += step.removed;
        match step.next {
            Some(cursor) => resume = Some(cursor),
            None => break,
        }
        assert!(steps < 1000, "bounded edge purge did not converge");
    }

    assert_eq!(
        removed_total, 4,
        "only (property 88, label 3) postings purged"
    );
    assert!(
        store
            .lookup_edge_equal(88, b"e", Some(3))
            .expect("lookup label 3")
            .is_empty()
    );
    assert_eq!(
        store
            .lookup_edge_equal(88, b"e", Some(7))
            .expect("lookup label 7")
            .len(),
        4
    );
    assert_eq!(
        store
            .lookup_edge_equal(89, b"e", Some(3))
            .expect("lookup property 89")
            .len(),
        4
    );
}

#[test]
fn admin_purge_property_postings_requires_router_caller() {
    let store = IndexStore::new();
    let _router = init_test_store(&store);
    let intruder = Principal::from_slice(&[200]);
    assert_eq!(
        store
            .admin_purge_property_postings(intruder, IndexPurgeKind::Vertex, 42, 0, None)
            .err(),
        Some(IndexError::NotAuthorized)
    );
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

    let result = store
        .lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(1, b"alice".to_vec()),
                IndexEqualSpec::vertex(2, b"a@b.c".to_vec()),
            ],
        })
        .expect("lookup_intersection");
    assert_eq!(
        result,
        IndexIntersectionResult::Vertices(vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 20
        }])
    );
}

#[test]
fn filter_hits_by_equal_keeps_arm_members_only() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    store
        .posting_insert(shard_principal, ShardId::new(0), 2, b"a@b.c".to_vec(), 20)
        .expect("email v20");
    store
        .posting_insert(shard_principal, ShardId::new(0), 2, b"a@b.c".to_vec(), 30)
        .expect("email v30");

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
    let filtered = store
        .filter_hits_by_equal(2, b"a@b.c", hits)
        .expect("filter_hits_by_equal");
    assert_eq!(
        filtered,
        vec![
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 20
            },
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 30
            },
        ]
    );
}

#[test]
fn filter_hits_by_equal_sorts_unsorted_input() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for v in [10u32, 20, 30] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 2, b"a@b.c".to_vec(), v)
            .expect("arm insert");
    }

    // Descending input must still be sieved correctly by the merge-join (it sorts internally).
    let hits = vec![
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 30,
        },
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 25,
        },
        PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 10,
        },
    ];
    let filtered = store
        .filter_hits_by_equal(2, b"a@b.c", hits)
        .expect("filter_hits_by_equal");
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
fn paged_walk_plus_equal_sieve_matches_lookup_intersection() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for v in [10u32, 20, 30, 40] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 1, b"alice".to_vec(), v)
            .expect("arm 1");
    }
    for v in [20u32, 30] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 2, b"a@b.c".to_vec(), v)
            .expect("arm 2");
    }

    let IndexIntersectionResult::Vertices(mut expected) = store
        .lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(1, b"alice".to_vec()),
                IndexEqualSpec::vertex(2, b"a@b.c".to_vec()),
            ],
        })
        .expect("lookup_intersection")
    else {
        panic!("expected vertex intersection");
    };
    expected.sort_by_key(|hit| (hit.shard_id, hit.vertex_id));

    // Stream the first arm in pages of 1 and sieve the second arm via `contains`,
    // mirroring the router/graph streaming composition (no full-bucket materialization).
    let mut streamed = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .lookup_equal_page(&LookupEqualPageRequest {
                property_id: 1,
                value: b"alice".to_vec(),
                after,
                limit: 1,
            })
            .expect("lookup_equal_page");
        let survivors = store
            .filter_hits_by_equal(2, b"a@b.c", page.hits)
            .expect("filter_hits_by_equal");
        streamed.extend(survivors);
        if page.done {
            break;
        }
        after = page.next;
    }

    // The paged walk emits hits in `(shard, vertex)` order; the materializing baseline is unordered.
    assert_eq!(streamed, expected);
}

#[test]
fn lookup_intersection_page_paginates_and_matches_lookup_intersection() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);

    for v in [10u32, 20, 30, 40] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 1, b"alice".to_vec(), v)
            .expect("arm 1");
    }
    for v in [20u32, 30] {
        store
            .posting_insert(shard_principal, ShardId::new(0), 2, b"a@b.c".to_vec(), v)
            .expect("arm 2");
    }

    let IndexIntersectionResult::Vertices(mut expected) = store
        .lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(1, b"alice".to_vec()),
                IndexEqualSpec::vertex(2, b"a@b.c".to_vec()),
            ],
        })
        .expect("lookup_intersection")
    else {
        panic!("expected vertex intersection");
    };
    expected.sort_by_key(|hit| (hit.shard_id, hit.vertex_id));

    // Drive the server-side paged endpoint with a 1-hit page so the walk arm spans multiple
    // pages, including pages that yield zero survivors after the sieve.
    let mut streamed = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .lookup_intersection_page(&LookupIntersectionPageRequest {
                specs: vec![
                    IndexEqualSpec::vertex(1, b"alice".to_vec()),
                    IndexEqualSpec::vertex(2, b"a@b.c".to_vec()),
                ],
                after,
                limit: 1,
            })
            .expect("lookup_intersection_page");
        streamed.extend(page.hits);
        if page.done {
            break;
        }
        after = page.next;
    }

    // The paged walk emits hits in `(shard, vertex)` order; the materializing baseline is unordered.
    assert_eq!(streamed, expected);
}

#[test]
fn lookup_intersection_page_empty_for_fewer_than_two_specs() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_principal = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_principal);
    store
        .posting_insert(shard_principal, ShardId::new(0), 1, b"alice".to_vec(), 10)
        .expect("arm");

    let page = store
        .lookup_intersection_page(&LookupIntersectionPageRequest {
            specs: vec![IndexEqualSpec::vertex(1, b"alice".to_vec())],
            after: None,
            limit: 16,
        })
        .expect("lookup_intersection_page");
    assert!(page.done);
    assert!(page.hits.is_empty());
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

    let result = store
        .lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(1, b"alice".to_vec()),
                IndexEqualSpec::vertex(2, b"bob".to_vec()),
            ],
        })
        .expect("lookup_intersection");
    assert_eq!(result, IndexIntersectionResult::Vertices(vec![]));
}

#[test]
fn lookup_intersection_requires_two_specs() {
    let store = IndexStore::new();
    let result = store
        .lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![IndexEqualSpec::vertex(1, b"x".to_vec())],
        })
        .expect("lookup_intersection");
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

    let result = store
        .lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(10, b"30".to_vec()),
                IndexEqualSpec::edge(20, b"5".to_vec(), Some(7)),
            ],
        })
        .expect("lookup_intersection");
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

    let result = store
        .lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::edge(30, b"1".to_vec(), Some(9)),
                IndexEqualSpec::edge(31, b"2".to_vec(), Some(9)),
            ],
        })
        .expect("lookup_intersection");
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
fn lookup_equal_page_paginates_and_resumes() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_a = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_a);

    for vid in [1u32, 2, 3] {
        store
            .posting_insert(shard_a, ShardId::new(0), 42, b"v".to_vec(), vid)
            .expect("insert");
    }

    let page1 = store
        .lookup_equal_page(&LookupEqualPageRequest {
            property_id: 42,
            value: b"v".to_vec(),
            after: None,
            limit: 2,
        })
        .expect("page1");
    assert_eq!(page1.hits.len(), 2);
    assert!(!page1.done);
    assert_eq!(
        page1.next,
        Some(PropertyPostingCursor {
            value: b"v".to_vec(),
            shard_id: ShardId::new(0),
            vertex_id: 2,
        })
    );

    let page2 = store
        .lookup_equal_page(&LookupEqualPageRequest {
            property_id: 42,
            value: b"v".to_vec(),
            after: page1.next,
            limit: 2,
        })
        .expect("page2");
    assert_eq!(
        page2.hits,
        vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 3,
        }]
    );
    assert!(page2.done);
    assert_eq!(page2.next, None);
}

#[test]
fn lookup_range_page_walks_values_across_pages() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_a = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_a);

    for (vid, val) in [
        (100u32, vec![1u8]),
        (200u32, vec![2u8]),
        (300u32, vec![3u8]),
    ] {
        store
            .posting_insert(shard_a, ShardId::new(0), 42, val, vid)
            .expect("insert");
    }

    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .lookup_range_page(&LookupRangePageRequest {
                property_id: 42,
                range: PostingRangeRequest::Ge(vec![1]),
                after,
                limit: 1,
            })
            .expect("range page");
        seen.extend(page.hits.iter().map(|h| h.vertex_id));
        if page.done {
            break;
        }
        after = page.next;
    }
    assert_eq!(seen, vec![100, 200, 300]);
}

#[test]
fn lookup_edge_equal_page_paginates_and_resumes() {
    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard_a = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard_a);

    for slot in [0u32, 1, 2] {
        store
            .edge_posting_insert(shard_a, ShardId::new(0), 88, b"e".to_vec(), 3, 10, slot)
            .expect("insert edge posting");
    }

    let page1 = store
        .lookup_edge_equal_page(&LookupEdgeEqualPageRequest {
            property_id: 88,
            value: b"e".to_vec(),
            label_id: Some(3),
            after: None,
            limit: 2,
        })
        .expect("page1");
    assert_eq!(page1.hits.len(), 2);
    assert!(!page1.done);
    assert_eq!(
        page1.next,
        Some(EdgePostingCursor {
            value: b"e".to_vec(),
            label_id: 3,
            shard_id: ShardId::new(0),
            owner_vertex_id: 10,
            slot_index: 1,
        })
    );

    let page2 = store
        .lookup_edge_equal_page(&LookupEdgeEqualPageRequest {
            property_id: 88,
            value: b"e".to_vec(),
            label_id: Some(3),
            after: page1.next,
            limit: 2,
        })
        .expect("page2");
    assert_eq!(page2.hits.len(), 1);
    assert_eq!(page2.hits[0].slot_index, 2);
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

    let hits = store
        .lookup_edge_equal(property_id, &value, Some(9))
        .expect("lookup_edge_equal");
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
    let remaining = store
        .lookup_edge_equal(property_id, &value, None)
        .expect("lookup_edge_equal");
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
        store
            .lookup_edge_equal(property_id, &value, Some(1))
            .expect("lookup_edge_equal")
            .len(),
        1
    );
    assert_eq!(
        store
            .lookup_edge_equal(property_id, &value, None)
            .expect("lookup_edge_equal")
            .len(),
        2
    );
}

fn bytes_index_key_of_len(len: usize) -> Vec<u8> {
    assert!(len >= 3);
    index_key(Value::Bytes(vec![1u8; len - 3]))
}

#[test]
fn posting_insert_accepts_at_limit_key_and_rejects_over_limit_without_stable_mutation() {
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard = Principal::from_slice(&[1]);
    attach_shard_canister(&store, router, ShardId::new(0), shard);

    let at_limit = bytes_index_key_of_len(MAX_INDEX_VALUE_KEY_BYTES);
    let over_limit = bytes_index_key_of_len(MAX_INDEX_VALUE_KEY_BYTES + 1);
    assert_eq!(at_limit.len(), MAX_INDEX_VALUE_KEY_BYTES);
    assert_eq!(over_limit.len(), MAX_INDEX_VALUE_KEY_BYTES + 1);

    store
        .posting_insert(shard, ShardId::new(0), 1, at_limit.clone(), 10)
        .expect("at-limit insert");
    assert_eq!(
        store
            .lookup_equal(1, &at_limit)
            .expect("lookup_equal")
            .len(),
        1
    );

    assert_eq!(
        store.posting_insert(shard, ShardId::new(0), 1, over_limit.clone(), 11),
        Err(IndexError::IndexValueKeyTooLarge)
    );
    assert_eq!(
        store.lookup_equal(1, &over_limit),
        Err(IndexError::IndexValueKeyTooLarge)
    );
    assert_eq!(
        store
            .lookup_equal(1, &at_limit)
            .expect("lookup_equal")
            .len(),
        1
    );
}

#[test]
fn edge_posting_insert_rejects_over_limit_without_stable_mutation() {
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    let store = IndexStore::new();
    let router = init_test_store(&store);
    let owner = Principal::from_slice(&[2]);
    attach_shard_canister(&store, router, ShardId::new(0), owner);

    let over_limit = bytes_index_key_of_len(MAX_INDEX_VALUE_KEY_BYTES + 1);
    assert_eq!(
        store.edge_posting_insert(owner, ShardId::new(0), 9, over_limit.clone(), 3, 10, 0),
        Err(IndexError::IndexValueKeyTooLarge)
    );
    assert_eq!(
        store.lookup_edge_equal(9, &over_limit, None),
        Err(IndexError::IndexValueKeyTooLarge)
    );
}

#[test]
fn posting_remove_accepts_oversized_key_for_legacy_cleanup() {
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard = Principal::from_slice(&[3]);
    attach_shard_canister(&store, router, ShardId::new(0), shard);

    let oversized = bytes_index_key_of_len(MAX_INDEX_VALUE_KEY_BYTES + 1);
    let legacy_key = crate::key::PostingKey {
        property_id: 5,
        value: oversized.clone(),
        shard_id: ShardId::new(0),
        vertex_id: 7,
    };
    INDEX_VERTEX_POSTINGS.with_borrow_mut(|postings| {
        postings.insert(legacy_key.clone());
    });

    store
        .posting_remove(shard, ShardId::new(0), 5, oversized.clone(), 7)
        .expect("remove oversized legacy posting");
    assert!(!INDEX_VERTEX_POSTINGS.with_borrow(|postings| postings.contains(&legacy_key)));
    assert_eq!(
        store.lookup_equal(5, &oversized),
        Err(IndexError::IndexValueKeyTooLarge)
    );
}

#[test]
fn edge_posting_remove_accepts_oversized_key_for_legacy_cleanup() {
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    let store = IndexStore::new();
    let router = init_test_store(&store);
    let shard = Principal::from_slice(&[5]);
    attach_shard_canister(&store, router, ShardId::new(0), shard);

    let oversized = bytes_index_key_of_len(MAX_INDEX_VALUE_KEY_BYTES + 1);
    let legacy_key = crate::edge_key::EdgePostingKey {
        property_id: 9,
        value: oversized.clone(),
        label_id: 3,
        shard_id: ShardId::new(0),
        owner_vertex_id: 10,
        slot_index: 0,
    };
    INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| {
        postings.insert(legacy_key.clone());
    });

    store
        .edge_posting_remove(shard, ShardId::new(0), 9, oversized, 3, 10, 0)
        .expect("remove oversized legacy edge posting");
    assert!(!INDEX_EDGE_POSTINGS.with_borrow(|postings| postings.contains(&legacy_key)));
}

#[test]
fn read_boundaries_reject_oversized_keys_without_false_empty_range() {
    use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

    let store = IndexStore::new();
    init_test_store(&store);
    let oversized = bytes_index_key_of_len(MAX_INDEX_VALUE_KEY_BYTES + 1);

    assert_eq!(
        store.lookup_equal(1, &oversized),
        Err(IndexError::IndexValueKeyTooLarge)
    );
    assert_eq!(
        store.lookup_edge_equal(2, &oversized, None),
        Err(IndexError::IndexValueKeyTooLarge)
    );
    assert_eq!(
        store.lookup_range(3, &PostingRangeRequest::Ge(oversized.clone())),
        Err(IndexError::IndexValueKeyTooLarge)
    );
    assert_eq!(
        store.lookup_intersection(&gleaph_graph_kernel::index::IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(1, b"ok".to_vec()),
                IndexEqualSpec::vertex(2, oversized),
            ],
        }),
        Err(IndexError::IndexValueKeyTooLarge)
    );
}
