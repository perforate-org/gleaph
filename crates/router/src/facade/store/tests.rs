use super::super::stable::label_stats::{
    ClientMutationKey, LabelStats, RouterMutationRecord, RouterMutationShard,
};
use super::*;
use crate::init::RouterInitArgs;
use crate::types::{AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState};
use candid::Principal;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::{NodeLabelRef, PhysicalPlan, PlanOp};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    LabelStatsDelta, LabelStatsDeltaEventWire, ResolvedLabelTable, ResolvedPropertyTable,
};
use std::collections::BTreeSet;

use crate::facade::stable::graph_catalog::lookup_graph_id;
use crate::facade::store::registry_invariants::assert_registry_invariants;

fn graph_principal(byte: u8) -> Principal {
    Principal::self_authenticating([byte; 32])
}

fn test_init_args() -> RouterInitArgs {
    RouterInitArgs {
        issuing_principal: Principal::anonymous(),
        initial_admins: vec![],
    }
}

fn register_test_graph(store: &RouterStore, admin: Principal, name: &str) {
    store
        .admin_register_graph(
            admin,
            GraphRegistryEntry {
                graph_id: GraphId::from_raw(0),
                graph_name: name.to_owned(),
                canister_id: Principal::management_canister(),
                owner: admin,
                admins: BTreeSet::new(),
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::None,
                is_home: false,
            },
        )
        .expect("register graph");
    assert_registry_invariants();
}

fn tenant_main_graph_id() -> GraphId {
    lookup_graph_id("tenant.main").expect("tenant.main")
}

#[test]
fn list_shards_for_graph_returns_matching_registrations() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    register_test_graph(&store, admin, "other.graph");

    let graph_a = graph_principal(1);
    let graph_b = graph_principal(4);
    let graph_c = graph_principal(5);
    let index = graph_principal(2);

    for (shard_id, graph) in [
        (ShardId::new(0), graph_a),
        (ShardId::new(1), graph_c),
        (ShardId::new(2), graph_b),
    ] {
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id,
                graph_canister: graph,
                index_canister: index,
                logical_graph_name: if shard_id != ShardId::new(2) {
                    "tenant.main".into()
                } else {
                    "other.graph".into()
                },
            },
        ))
        .expect("register");
    }

    let listed = store.list_shards_for_graph("tenant.main").expect("list");
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|e| e.shard_id == ShardId::new(0)));
    assert!(listed.iter().any(|e| e.shard_id == ShardId::new(1)));
}

#[test]
fn unregister_shard_removes_registry_and_leaves_siblings() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let graph_a = graph_principal(1);
    let graph_b = graph_principal(4);
    let index = graph_principal(2);

    for (shard_id, graph) in [(ShardId::new(0), graph_a), (ShardId::new(1), graph_b)] {
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id,
                graph_canister: graph,
                index_canister: index,
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register");
    }

    futures::executor::block_on(store.admin_unregister_shard(admin, ShardId::new(0)))
        .expect("unregister");

    let listed = store.list_shards_for_graph("tenant.main").expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].shard_id, ShardId::new(1));
    assert_eq!(listed[0].graph_canister, graph_b);
    assert!(store.resolve_shard(ShardId::new(0)).is_err());
    assert!(store.resolve_shard(ShardId::new(1)).is_ok());
    assert_registry_invariants();
}

#[test]
fn registry_invariants_hold_after_graph_and_shard_register() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard");

    assert_registry_invariants();
}

#[test]
fn list_shards_for_graph_fails_on_stale_shard_index() {
    use crate::facade::stable::ROUTER_SHARDS_BY_GRAPH_ID;

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard");

    let graph_id = tenant_main_graph_id();
    ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
        let mut list = index.get(&graph_id).unwrap_or_default();
        list.shard_ids.push(ShardId::new(99));
        index.insert(graph_id, list);
    });

    let err = store
        .list_shards_for_graph("tenant.main")
        .expect_err("stale index must fail");
    assert!(matches!(err, RouterError::Internal(_)));
}

#[test]
fn check_registry_invariants_fails_when_shard_missing_from_index() {
    use super::registry_invariants::check_registry_invariants;
    use crate::facade::stable::ROUTER_SHARDS_BY_GRAPH_ID;

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard");

    let graph_id = tenant_main_graph_id();
    ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
        index.remove(&graph_id);
    });

    let err = check_registry_invariants().expect_err("missing index entry must fail invariants");
    assert!(err.contains("ROUTER_SHARDS_BY_GRAPH_ID"));
}

#[test]
fn admin_register_shard_rejects_orphan_catalog_graph() {
    use crate::facade::stable::graph_catalog::insert_graph_name;

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    insert_graph_name("orphan.graph", GraphId::from_raw(99)).expect("catalog insert");

    let err = futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "orphan.graph".into(),
        },
    ))
    .expect_err("orphan catalog graph");
    assert_eq!(err, RouterError::NotFound("orphan.graph".into()));
}

#[test]
fn admin_register_shard_rejects_same_id_under_different_graph() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    register_test_graph(&store, admin, "other.graph");

    let graph = graph_principal(1);
    let index = graph_principal(2);
    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph,
            index_canister: index,
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register under tenant.main");

    let err = futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph,
            index_canister: index,
            logical_graph_name: "other.graph".into(),
        },
    ))
    .expect_err("same shard under different graph");
    assert!(matches!(err, RouterError::Conflict(_)));
}

#[test]
fn list_shards_for_graph_fails_on_duplicate_shard_index() {
    use crate::facade::stable::ROUTER_SHARDS_BY_GRAPH_ID;

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard");

    let graph_id = tenant_main_graph_id();
    ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
        let mut list = index.get(&graph_id).unwrap_or_default();
        list.shard_ids.push(ShardId::new(0));
        index.insert(graph_id, list);
    });

    let err = store
        .list_shards_for_graph("tenant.main")
        .expect_err("duplicate index entry must fail");
    assert!(matches!(err, RouterError::Internal(_)));
}

#[test]
fn check_registry_invariants_rejects_orphan_catalog_entry() {
    use super::registry_invariants::check_registry_invariants;
    use crate::facade::stable::graph_catalog::insert_graph_name;

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    insert_graph_name("orphan.graph", GraphId::from_raw(99)).expect("catalog insert");

    let err = check_registry_invariants().expect_err("orphan catalog");
    assert!(err.contains("ROUTER_GRAPH_CATALOG"));
    assert!(err.contains("ROUTER_GRAPHS"));
}

#[test]
fn resolve_graph_checks_permissions() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    let owner = graph_principal(10);
    let other = graph_principal(11);

    store
        .admin_register_graph(
            admin,
            GraphRegistryEntry {
                graph_id: GraphId::from_raw(0),
                graph_name: "g".into(),
                canister_id: owner,
                owner,
                admins: BTreeSet::new(),
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::None,
                is_home: false,
            },
        )
        .expect("register");

    assert!(store.resolve_graph("g", owner).is_ok());
    assert_eq!(store.resolve_graph("g", other), Err(RouterError::Forbidden));
}

#[test]
fn vertex_and_edge_labels_with_same_name_get_distinct_ids() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);

    let v = store
        .admin_intern_vertex_label(admin, "Person")
        .expect("vertex label");
    let e = store
        .admin_intern_edge_label(admin, "Person")
        .expect("edge label");
    // Same numeric id is fine — namespaces are separate.
    assert_eq!(v.raw(), 1);
    assert_eq!(e.raw(), 1);
    assert_eq!(store.lookup_vertex_label_id("Person").unwrap(), v);
    assert_eq!(store.lookup_edge_label_id("Person").unwrap(), e);
    assert!(store.lookup_edge_label_id("KNOWS").is_err());
    let v2 = store
        .admin_intern_vertex_label(admin, "KNOWS")
        .expect("vertex only");
    assert_eq!(v2.raw(), 2);
}

#[test]
fn read_plan_requires_existing_label() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());

    let plan = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
        variable: "n".into(),
        label: Some(NodeLabelRef::from("Missing")),
        property_projection: None,
    }]);

    assert_eq!(
        store.resolve_plan_labels(&[plan]),
        Err(RouterError::NotFound("Missing".into()))
    );
}

#[test]
fn dml_plan_creates_only_requested_label_namespaces() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());

    let node_only = PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
        variable: Some("n".into()),
        labels: vec![NodeLabelRef::from("Person")],
        properties: vec![],
    }]);

    let resolved = store
        .resolve_plan_labels(&[node_only])
        .expect("resolve node DML labels");
    assert_eq!(resolved.vertex.len(), 1);
    assert_eq!(resolved.vertex[0].name, "Person");
    assert_eq!(resolved.vertex[0].id.raw(), 1);
    assert!(resolved.edge.is_empty());
    assert_eq!(store.lookup_vertex_label_id("Person").unwrap().raw(), 1);
    assert!(store.lookup_edge_label_id("Person").is_err());

    let edge_only = PhysicalPlan::from_ops(vec![PlanOp::InsertEdge {
        variable: Some("e".into()),
        src: "a".into(),
        dst: "b".into(),
        direction: EdgeDirection::PointingRight,
        labels: vec!["Person".into()],
        properties: vec![],
    }]);

    let resolved = store
        .resolve_plan_labels(&[edge_only])
        .expect("resolve edge DML labels");
    assert_eq!(resolved.edge.len(), 1);
    assert_eq!(resolved.edge[0].name, "Person");
    assert_eq!(resolved.edge[0].id.raw(), 1);
    assert_eq!(store.lookup_vertex_label_id("Person").unwrap().raw(), 1);
    assert_eq!(store.lookup_edge_label_id("Person").unwrap().raw(), 1);
}

#[test]
fn resolve_plan_attaches_edge_payload_profile() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);

    let profile = EdgePayloadProfile {
        byte_width: 2,
        encoding: EdgePayloadEncoding::WeightRawU16,
    };
    store
        .admin_intern_edge_label(admin, "KNOWS")
        .expect("intern edge");
    store
        .admin_set_edge_label_payload_profile(admin, "KNOWS", profile.clone())
        .expect("set profile");

    let edge_only = PhysicalPlan::from_ops(vec![PlanOp::InsertEdge {
        variable: Some("e".into()),
        src: "a".into(),
        dst: "b".into(),
        direction: EdgeDirection::PointingRight,
        labels: vec!["KNOWS".into()],
        properties: vec![],
    }]);

    let resolved = store
        .resolve_plan_labels(&[edge_only])
        .expect("resolve edge DML labels");
    assert_eq!(resolved.edge.len(), 1);
    assert_eq!(resolved.edge[0].name, "KNOWS");
    assert_eq!(resolved.edge[0].payload_profile, profile);
}

#[test]
fn label_stats_delta_updates_namespace_separated_stats() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);

    let vertex_label = store
        .admin_intern_vertex_label(admin, "Person")
        .expect("vertex label");
    let edge_label = store
        .admin_intern_edge_label(admin, "Person")
        .expect("edge label");

    store.apply_label_stats_delta_payload(
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(vertex_label, 2)],
            edge: vec![(edge_label, 3)],
        },
    );

    assert_eq!(
        store.vertex_label_stats(vertex_label),
        LabelStats {
            live_count: 2,
            total_adds: 2,
            total_removes: 0
        }
    );
    assert_eq!(
        store.edge_label_stats(edge_label),
        LabelStats {
            live_count: 3,
            total_adds: 3,
            total_removes: 0
        }
    );
    assert_eq!(
        store.vertex_label_shard_live_count(ShardId::new(0), vertex_label),
        2
    );
    assert_eq!(
        store.edge_label_shard_live_count(ShardId::new(0), edge_label),
        3
    );

    store.apply_label_stats_delta_payload(
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(vertex_label, -1)],
            edge: vec![(edge_label, -2)],
        },
    );

    assert_eq!(
        store.vertex_label_stats(vertex_label),
        LabelStats {
            live_count: 1,
            total_adds: 2,
            total_removes: 1
        }
    );
    assert_eq!(
        store.edge_label_stats(edge_label),
        LabelStats {
            live_count: 1,
            total_adds: 3,
            total_removes: 2
        }
    );
    assert_eq!(
        store.vertex_label_shard_live_count(ShardId::new(0), vertex_label),
        1
    );
    assert_eq!(
        store.edge_label_shard_live_count(ShardId::new(0), edge_label),
        1
    );
}

#[test]
fn label_stats_delta_tracks_per_shard_live_counts() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    let label = store
        .admin_intern_vertex_label(admin, "Person")
        .expect("vertex label");

    store.apply_label_stats_delta_payload(
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(label, 2)],
            edge: vec![],
        },
    );
    store.apply_label_stats_delta_payload(
        ShardId::new(1),
        &LabelStatsDelta {
            vertex: vec![(label, 1)],
            edge: vec![],
        },
    );
    store.apply_label_stats_delta_payload(
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(label, -1)],
            edge: vec![],
        },
    );

    assert_eq!(
        store.vertex_label_stats(label),
        LabelStats {
            live_count: 2,
            total_adds: 3,
            total_removes: 1
        }
    );
    assert_eq!(
        store.vertex_label_shard_live_count(ShardId::new(0), label),
        1
    );
    assert_eq!(
        store.vertex_label_shard_live_count(ShardId::new(1), label),
        1
    );
}

#[test]
fn label_stats_projection_applies_delta_once_per_seq() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    let label = store
        .admin_intern_vertex_label(admin, "Person")
        .expect("vertex label");
    let shard_id = ShardId::new(0);
    let graph = graph_principal(1);
    let deltas = vec![LabelStatsDeltaEventWire {
        mutation_id: 1,
        shard_event_seq: 1,
        label_stats_delta: LabelStatsDelta {
            vertex: vec![(label, 2)],
            edge: vec![],
        },
    }];

    futures::executor::block_on(store.advance_label_stats_projection(
        graph,
        shard_id,
        10,
        |_graph, from_seq, _limit| {
            assert_eq!(from_seq, 1);
            async { Ok(deltas.clone()) }
        },
        |_graph, through_seq| {
            assert_eq!(through_seq, 1);
            async { Ok(()) }
        },
    ))
    .expect("first advance");

    assert_eq!(
        store.vertex_label_stats(label),
        LabelStats {
            live_count: 2,
            total_adds: 2,
            total_removes: 0
        }
    );

    futures::executor::block_on(store.advance_label_stats_projection(
        graph,
        shard_id,
        10,
        |_graph, from_seq, _limit| {
            assert_eq!(from_seq, 2);
            async { Ok(Vec::new()) }
        },
        |_graph, _through_seq| async { Ok(()) },
    ))
    .expect("second advance");

    assert_eq!(
        store.vertex_label_stats(label),
        LabelStats {
            live_count: 2,
            total_adds: 2,
            total_removes: 0
        }
    );

    futures::executor::block_on(store.advance_label_stats_projection(
        graph,
        ShardId::new(1),
        10,
        |_graph, from_seq, _limit| {
            assert_eq!(from_seq, 1);
            async { Ok(deltas) }
        },
        |_graph, through_seq| {
            assert_eq!(through_seq, 1);
            async { Ok(()) }
        },
    ))
    .expect("other shard advance");

    assert_eq!(
        store.vertex_label_stats(label),
        LabelStats {
            live_count: 4,
            total_adds: 4,
            total_removes: 0
        }
    );
}

#[test]
fn mutation_id_is_monotonic_and_rejects_exhaustion() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());

    assert_eq!(store.allocate_mutation_id().expect("first"), 1);
    assert_eq!(store.allocate_mutation_id().expect("second"), 2);

    ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| {
        counter.set(u64::MAX);
    });
    assert_eq!(
        store.allocate_mutation_id(),
        Err(RouterError::IdExhausted("mutation_id".into()))
    );
}

#[test]
fn client_mutation_key_reuses_router_mutation_id() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);
    let request = b"request-a".to_vec();

    let first = store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            request.clone(),
        )
        .expect("first mutation id")
        .mutation_id;
    assert_eq!(first, 1);
    store
        .record_router_mutation_shards(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            vec![RouterMutationShard::new(
                ShardId::new(0),
                graph_principal(1),
                None,
            )],
        )
        .expect("record empty envelope");
    assert_eq!(
        store
            .reserve_mutation_id_for_client_key(
                caller,
                tenant_main_graph_id(),
                "client-key-1",
                request.clone()
            )
            .expect("retry mutation id")
            .mutation_id,
        first
    );
    assert_eq!(
        store
            .reserve_mutation_id_for_client_key(
                caller,
                tenant_main_graph_id(),
                "client-key-2",
                request.clone()
            )
            .expect("second mutation id")
            .mutation_id,
        2
    );
    assert_eq!(
        store
            .reserve_mutation_id_for_client_key(
                graph_principal(43),
                tenant_main_graph_id(),
                "client-key-1",
                request
            )
            .expect("different caller mutation id")
            .mutation_id,
        3
    );
}

#[test]
fn client_mutation_key_rejects_different_request() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);

    assert_eq!(
        store
            .reserve_mutation_id_for_client_key(
                caller,
                tenant_main_graph_id(),
                "client-key-1",
                b"a".to_vec()
            )
            .expect("first mutation id")
            .mutation_id,
        1
    );
    assert_eq!(
        store.reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"b".to_vec()
        ),
        Err(RouterError::Conflict(
            "client_mutation_key was already used for a different request".into()
        ))
    );
}

#[test]
fn client_mutation_key_rejects_expired_key() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);
    let graph_id =
        crate::facade::stable::graph_catalog::lookup_graph_id("tenant.main").expect("graph id");
    let key = ClientMutationKey::new(caller, graph_id, "client-key-1".into());
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
        m.insert(
            key,
            RouterMutationRecord::new(
                1,
                0u64.saturating_sub(CLIENT_MUTATION_KEY_TTL_NS + 1),
                b"a".to_vec(),
            ),
        );
    });

    assert_eq!(
        store.reserve_mutation_id_for_client_key_at(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
            CLIENT_MUTATION_KEY_TTL_NS + 1
        ),
        Err(RouterError::InvalidArgument(
            "client_mutation_key expired; use a new key for a new mutation".into()
        ))
    );
}

#[test]
fn client_mutation_key_blocks_concurrent_routing_owner() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);

    let first = store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        )
        .expect("first owner");
    assert_eq!(first.mutation_id, 1);
    assert!(first.routing_owner);

    assert_eq!(
        store.reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        ),
        Err(RouterError::Conflict(
            "client_mutation_key is already in progress; retry later".into()
        ))
    );

    store
        .record_router_mutation_shards(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            vec![RouterMutationShard::new(
                ShardId::new(0),
                graph_principal(1),
                None,
            )],
        )
        .expect("record envelope");
    let retry = store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        )
        .expect("retry after envelope");
    assert_eq!(retry.mutation_id, first.mutation_id);
    assert!(!retry.routing_owner);
}

#[test]
fn abandoned_routing_reservation_preserves_id_and_allows_new_owner() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);

    let first = store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        )
        .expect("first owner");
    assert_eq!(first.mutation_id, 1);
    assert!(first.routing_owner);

    store
        .abandon_router_mutation_routing_reservation(caller, tenant_main_graph_id(), "client-key-1")
        .expect("abandon reservation");
    let record = store
        .router_mutation_record(caller, tenant_main_graph_id(), "client-key-1")
        .expect("record");
    assert_eq!(record.mutation_id, first.mutation_id);
    assert_eq!(record.request_fingerprint, b"a".to_vec());
    assert!(!record.routing_in_progress);

    let retry = store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        )
        .expect("retry owner");
    assert_eq!(retry.mutation_id, first.mutation_id);
    assert!(retry.routing_owner);
}

#[test]
fn router_mutation_journal_tracks_shard_completion() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);
    store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        )
        .expect("mutation id");
    store
        .record_router_mutation_shards(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            vec![
                RouterMutationShard::new(ShardId::new(0), graph_principal(1), Some(vec![1])),
                RouterMutationShard::new(ShardId::new(1), graph_principal(2), None),
            ],
        )
        .expect("record shards");
    let record = store
        .router_mutation_record(caller, tenant_main_graph_id(), "client-key-1")
        .expect("record");
    assert_eq!(record.resolved_labels, Some(ResolvedLabelTable::default()));
    assert_eq!(
        store.router_mutation_completed_row_count(caller, tenant_main_graph_id(), "client-key-1"),
        None
    );

    store
        .record_router_mutation_shard_completed(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ShardId::new(0),
            2,
        )
        .expect("complete shard 0");
    store
        .record_router_mutation_shard_projection_advanced(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ShardId::new(0),
        )
        .expect("advance projection shard 0");
    assert_eq!(
        store.router_mutation_completed_row_count(caller, tenant_main_graph_id(), "client-key-1"),
        None
    );

    store
        .record_router_mutation_shard_completed(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ShardId::new(1),
            3,
        )
        .expect("complete shard 1");
    store
        .record_router_mutation_shard_projection_advanced(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ShardId::new(1),
        )
        .expect("advance projection shard 1");
    assert_eq!(
        store.router_mutation_completed_row_count(caller, tenant_main_graph_id(), "client-key-1"),
        Some(5)
    );
}

#[test]
fn router_mutation_journal_records_zero_shard_completion() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::anonymous();
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);
    store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        )
        .expect("mutation id");
    store
        .record_router_mutation_completed_without_shards(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            0,
        )
        .expect("record zero-shard completion");

    let record = store
        .router_mutation_record(caller, tenant_main_graph_id(), "client-key-1")
        .expect("record");
    assert_eq!(record.completed_row_count, Some(0));
    assert!(record.shards.is_empty());
    assert_eq!(
        store.router_mutation_completed_row_count(caller, tenant_main_graph_id(), "client-key-1"),
        Some(0)
    );
}
