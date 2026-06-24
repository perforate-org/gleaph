use super::super::stable::label_stats::{
    ClientMutationKey, LabelStats, RouterMutationRecord, RouterMutationShard,
};
use super::*;
use crate::init::RouterInitArgs;
use crate::types::{
    AdminAttachVectorIndexShardArgs, AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus,
    ProvisioningState,
};
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

pub(crate) fn register_test_graph(store: &RouterStore, admin: Principal, name: &str) {
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
    let admin = Principal::from_slice(&[1; 29]);
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
        (ShardId::new(0), graph_b),
    ] {
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id,
                graph_canister: graph,
                index_canister: index,
                logical_graph_name: if graph != graph_b {
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
    let admin = Principal::from_slice(&[1; 29]);
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

    futures::executor::block_on(store.admin_unregister_shard(
        admin,
        "tenant.main",
        ShardId::new(0),
    ))
    .expect("unregister");

    let listed = store.list_shards_for_graph("tenant.main").expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].shard_id, ShardId::new(1));
    assert_eq!(listed[0].graph_canister, graph_b);
    let graph_id = tenant_main_graph_id();
    assert!(store.resolve_shard(graph_id, ShardId::new(0)).is_err());
    assert!(store.resolve_shard(graph_id, ShardId::new(1)).is_ok());
    assert_registry_invariants();
}

#[test]
fn unregister_shard_prunes_graph_index_lookup_targets_to_live_shards() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard 0");
    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(1),
            graph_canister: graph_principal(3),
            index_canister: graph_principal(4),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard 1");
    let targets = store.graph_index_lookup_targets(graph_id).expect("targets");
    assert_eq!(targets.len(), 2);
    assert!(targets.contains(&graph_principal(2)));
    assert!(targets.contains(&graph_principal(4)));

    futures::executor::block_on(store.admin_unregister_shard(
        admin,
        "tenant.main",
        ShardId::new(1),
    ))
    .expect("unregister shard 1");

    assert_eq!(
        store.graph_index_lookup_targets(graph_id).expect("targets"),
        vec![graph_principal(2)]
    );
}

#[test]
fn admin_register_graph_with_random_key_persists_runtime_config() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    let entry = GraphRegistryEntry {
        graph_id: GraphId::from_raw(0),
        graph_name: "tenant.main".to_owned(),
        canister_id: Principal::management_canister(),
        owner: admin,
        admins: BTreeSet::new(),
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: 0,
        provisioning_state: ProvisioningState::None,
        is_home: false,
    };

    futures::executor::block_on(store.admin_register_graph_with_random_key(admin, entry))
        .expect("register graph");
    let graph_id = lookup_graph_id("tenant.main").expect("graph id");
    let key = store
        .graph_element_id_encoding_key(graph_id)
        .expect("runtime key");
    assert_ne!(
        key,
        gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture()
    );
}

#[test]
fn admin_register_graph_derives_distinct_element_id_keys_per_graph() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);

    for (name, is_home) in [("graph_a", true), ("graph_b", false)] {
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
                    is_home,
                },
            )
            .expect("register graph");
    }

    let key_a = store
        .graph_element_id_encoding_key(lookup_graph_id("graph_a").expect("graph a"))
        .expect("key a");
    let key_b = store
        .graph_element_id_encoding_key(lookup_graph_id("graph_b").expect("graph b"))
        .expect("key b");
    assert_ne!(key_a, key_b);
}

#[test]
fn register_shard_extends_runtime_index_cluster_by_group() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard 0");
    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(1),
            graph_canister: graph_principal(3),
            index_canister: graph_principal(4),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard 1");

    let runtime = crate::facade::stable::ROUTER_GRAPH_RUNTIME_CONFIG
        .with_borrow(|cfg| cfg.get(&graph_id))
        .expect("runtime config");
    assert_eq!(runtime.index_group_size, 1);
    assert_eq!(
        runtime.index_cluster,
        vec![graph_principal(2), graph_principal(4)]
    );
}

#[test]
fn register_shard_rejects_index_canister_mismatch_within_group() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();
    crate::facade::stable::ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow_mut(|cfg| {
        let mut runtime = cfg.get(&graph_id).expect("runtime config");
        runtime.index_group_size = 2;
        cfg.insert(graph_id, runtime);
    });

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard 0");

    let err = futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(1),
            graph_canister: graph_principal(3),
            index_canister: graph_principal(4),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect_err("group index canister mismatch");
    assert!(matches!(err, RouterError::Conflict(_)));
}

#[test]
fn unregister_graph_rejects_when_shards_exist() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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

    let err = store
        .admin_unregister_graph(admin, "tenant.main")
        .expect_err("must reject while shards exist");
    assert!(matches!(err, RouterError::Conflict(_)));
}

#[test]
fn unregister_graph_cascades_vocabulary_partitions() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();
    let edge = store
        .admin_intern_edge_label(admin, "tenant.main", "KNOWS")
        .expect("edge label");
    let vertex = store
        .admin_intern_vertex_label(admin, "tenant.main", "Person")
        .expect("vertex label");
    store
        .admin_intern_property(admin, "tenant.main", "age")
        .expect("property");
    store
        .admin_set_edge_label_payload_profile(
            admin,
            "tenant.main",
            "KNOWS",
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .expect("payload profile");
    store.apply_label_stats_delta_payload(
        graph_id,
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(vertex, 2)],
            edge: vec![(edge, 1)],
        },
    );

    store
        .admin_unregister_graph(admin, "tenant.main")
        .expect("unregister graph");

    assert!(store.resolve_graph_id("tenant.main").is_err());
    register_test_graph(&store, admin, "tenant.main");
    let new_graph_id = tenant_main_graph_id();
    assert_eq!(
        store
            .admin_intern_vertex_label(admin, "tenant.main", "Person")
            .expect("vertex re-intern")
            .raw(),
        1
    );
    assert_eq!(
        store
            .admin_intern_edge_label(admin, "tenant.main", "KNOWS")
            .expect("edge re-intern")
            .raw(),
        1
    );
    assert_eq!(
        store
            .admin_intern_property(admin, "tenant.main", "age")
            .expect("property re-intern")
            .raw(),
        1
    );
    assert_eq!(
        store.vertex_label_stats(new_graph_id, vertex),
        LabelStats::default()
    );
    assert_eq!(
        store.edge_label_stats(new_graph_id, edge),
        LabelStats::default()
    );
    assert_eq!(
        store.vertex_label_shard_live_count(new_graph_id, ShardId::new(0), vertex),
        0
    );
    assert_eq!(
        store.edge_label_shard_live_count(new_graph_id, ShardId::new(0), edge),
        0
    );
    assert!(store.lookup_edge_label_id(new_graph_id, "KNOWS").is_ok());
    assert!(store.lookup_property_id(new_graph_id, "age").is_ok());
    assert_registry_invariants();
}

#[test]
fn registry_invariants_hold_after_graph_and_shard_register() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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
fn admin_register_shard_allows_same_ordinal_under_different_graphs() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    register_test_graph(&store, admin, "other.graph");

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register under tenant.main");

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(3),
            index_canister: graph_principal(4),
            logical_graph_name: "other.graph".into(),
        },
    ))
    .expect("same graph-local ordinal under other.graph");

    assert_registry_invariants();
}

#[test]
fn admin_register_shard_rejects_wrong_shard_id_for_existing_graph_canister() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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
    .expect("register shard 0");

    let err = futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(1),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect_err("wrong shard id must not succeed");

    assert!(matches!(err, RouterError::Conflict(_)));
    assert_registry_invariants();
}

#[test]
fn pending_shard_excluded_from_index_lookup_targets() {
    use crate::facade::stable::ROUTER_SHARDS;
    use gleaph_graph_kernel::federation::GraphShardKey;

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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
    assert_eq!(
        store.graph_index_lookup_targets(graph_id).expect("targets"),
        vec![graph_principal(2)]
    );

    ROUTER_SHARDS.with_borrow_mut(|shards| {
        let key = GraphShardKey::new(graph_id, ShardId::new(0));
        let mut entry = shards.get(&key).expect("shard row");
        entry.index_attached = false;
        shards.insert(key, entry);
    });
    assert!(
        store
            .graph_index_lookup_targets(graph_id)
            .expect("targets")
            .is_empty()
    );
}

#[test]
fn unregister_shard_reconciles_index_cluster_for_retry() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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

    futures::executor::block_on(store.admin_unregister_shard(
        admin,
        "tenant.main",
        ShardId::new(0),
    ))
    .expect("unregister shard");

    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(3),
            index_canister: graph_principal(4),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("re-register with different index canister");

    assert_registry_invariants();
}

#[test]
fn admin_register_graph_with_random_key_rejects_duplicate_home_after_first() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);

    store
        .admin_register_graph(
            admin,
            GraphRegistryEntry {
                graph_id: GraphId::from_raw(0),
                graph_name: "home.graph".into(),
                canister_id: Principal::management_canister(),
                owner: admin,
                admins: BTreeSet::new(),
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::None,
                is_home: true,
            },
        )
        .expect("register home");

    let err = futures::executor::block_on(store.admin_register_graph_with_random_key(
        admin,
        GraphRegistryEntry {
            graph_id: GraphId::from_raw(0),
            graph_name: "other.home".into(),
            canister_id: Principal::management_canister(),
            owner: admin,
            admins: BTreeSet::new(),
            status: GraphStatus::Active,
            version: 1,
            updated_at_ns: 0,
            provisioning_state: ProvisioningState::None,
            is_home: true,
        },
    ))
    .expect_err("second home graph");

    assert!(matches!(err, RouterError::Conflict(_)));
}

#[test]
fn admin_register_shard_rejects_duplicate_graph_local_ordinal() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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
    .expect("register shard 0");

    let err = futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(3),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect_err("second shard 0 must fail");

    assert!(matches!(err, RouterError::Conflict(_)));
    assert_registry_invariants();
}

#[test]
fn list_shards_for_graph_fails_on_duplicate_shard_index() {
    use crate::facade::stable::ROUTER_SHARDS_BY_GRAPH_ID;

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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
    // Existence non-disclosure: a non-tenant gets NotFound, not Forbidden, so it cannot
    // distinguish "exists but forbidden" from "does not exist".
    assert_eq!(
        store.resolve_graph("g", other),
        Err(RouterError::NotFound("g".into()))
    );
    // Superuser bypass: a global canister Admin may resolve any graph by name.
    assert!(store.resolve_graph("g", admin).is_ok());
}

#[test]
fn resolve_graph_id_authorized_enforces_tenancy() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    let owner = graph_principal(10);
    let member = graph_principal(12);
    let other = graph_principal(11);

    let mut admins = BTreeSet::new();
    admins.insert(member);
    store
        .admin_register_graph(
            admin,
            GraphRegistryEntry {
                graph_id: GraphId::from_raw(0),
                graph_name: "g".into(),
                canister_id: owner,
                owner,
                admins,
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::None,
                is_home: false,
            },
        )
        .expect("register");

    let gid = lookup_graph_id("g").expect("g");
    assert_eq!(store.resolve_graph_id_authorized("g", owner), Ok(gid));
    assert_eq!(store.resolve_graph_id_authorized("g", member), Ok(gid));
    // Superuser bypass.
    assert_eq!(store.resolve_graph_id_authorized("g", admin), Ok(gid));
    // Non-tenant cannot even confirm existence.
    assert_eq!(
        store.resolve_graph_id_authorized("g", other),
        Err(RouterError::NotFound("g".into()))
    );
    // Unknown graph is indistinguishable from a forbidden one.
    assert_eq!(
        store.resolve_graph_id_authorized("missing", other),
        Err(RouterError::NotFound("missing".into()))
    );
}

#[test]
fn resolve_graph_id_authorized_allows_registered_shard_canister() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    register_test_graph(&store, admin, "other.graph");

    let shard_canister = graph_principal(1);
    let index = graph_principal(2);
    futures::executor::block_on(store.admin_register_shard(
        admin,
        AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: shard_canister,
            index_canister: index,
            logical_graph_name: "tenant.main".into(),
        },
    ))
    .expect("register shard");

    let tenant = lookup_graph_id("tenant.main").expect("tenant.main");
    // A graph's own registered shard canister may resolve its routing metadata
    // (keeps federation/index-routing inter-canister calls working).
    assert_eq!(
        store.resolve_graph_id_authorized("tenant.main", shard_canister),
        Ok(tenant)
    );
    // ...but not a different graph it is not a shard of.
    assert_eq!(
        store.resolve_graph_id_authorized("other.graph", shard_canister),
        Err(RouterError::NotFound("other.graph".into()))
    );
    // An unrelated principal is rejected entirely.
    assert_eq!(
        store.resolve_graph_id_authorized("tenant.main", graph_principal(99)),
        Err(RouterError::NotFound("tenant.main".into()))
    );
}

#[test]
fn register_graph_rejects_anonymous_owner_and_admin() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);

    let anon_owner = GraphRegistryEntry {
        graph_id: GraphId::from_raw(0),
        graph_name: "g".into(),
        canister_id: Principal::management_canister(),
        owner: Principal::anonymous(),
        admins: BTreeSet::new(),
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: 0,
        provisioning_state: ProvisioningState::None,
        is_home: false,
    };
    assert!(matches!(
        store.admin_register_graph(admin, anon_owner),
        Err(RouterError::InvalidArgument(_))
    ));
    // Rejected before any state mutation: the name was never interned.
    assert!(lookup_graph_id("g").is_none());

    let owner = graph_principal(10);
    let mut admins = BTreeSet::new();
    admins.insert(Principal::anonymous());
    let anon_admin = GraphRegistryEntry {
        graph_id: GraphId::from_raw(0),
        graph_name: "g2".into(),
        canister_id: owner,
        owner,
        admins,
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: 0,
        provisioning_state: ProvisioningState::None,
        is_home: false,
    };
    assert!(matches!(
        store.admin_register_graph(admin, anon_admin),
        Err(RouterError::InvalidArgument(_))
    ));
}

#[test]
fn vertex_and_edge_labels_with_same_name_get_distinct_ids() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);

    register_test_graph(&store, admin, "tenant.main");

    let v = store
        .admin_intern_vertex_label(admin, "tenant.main", "Person")
        .expect("vertex label");
    let e = store
        .admin_intern_edge_label(admin, "tenant.main", "Person")
        .expect("edge label");
    // Same numeric id is fine — namespaces are separate.
    assert_eq!(v.raw(), 1);
    assert_eq!(e.raw(), 1);
    assert_eq!(
        store
            .lookup_vertex_label_id(tenant_main_graph_id(), "Person")
            .unwrap(),
        v
    );
    assert_eq!(
        store
            .lookup_edge_label_id(tenant_main_graph_id(), "Person")
            .unwrap(),
        e
    );
    assert!(
        store
            .lookup_edge_label_id(tenant_main_graph_id(), "KNOWS")
            .is_err()
    );
    let v2 = store
        .admin_intern_vertex_label(admin, "tenant.main", "KNOWS")
        .expect("vertex only");
    assert_eq!(v2.raw(), 2);
}

#[test]
fn read_plan_requires_existing_label() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let plan = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
        variable: "n".into(),
        label: Some(NodeLabelRef::from("Missing")),
        property_projection: None,
    }]);

    assert_eq!(
        store.resolve_plan_labels(tenant_main_graph_id(), &[plan]),
        Err(RouterError::NotFound("Missing".into()))
    );
}

#[test]
fn dml_plan_creates_only_requested_label_namespaces() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let node_only = PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
        variable: Some("n".into()),
        labels: vec![NodeLabelRef::from("Person")],
        properties: vec![],
    }]);

    let resolved = store
        .resolve_plan_labels(tenant_main_graph_id(), &[node_only])
        .expect("resolve node DML labels");
    assert_eq!(resolved.vertex.len(), 1);
    assert_eq!(resolved.vertex[0].name, "Person");
    assert_eq!(resolved.vertex[0].id.raw(), 1);
    assert!(resolved.edge.is_empty());
    assert_eq!(
        store
            .lookup_vertex_label_id(tenant_main_graph_id(), "Person")
            .unwrap()
            .raw(),
        1
    );
    assert!(
        store
            .lookup_edge_label_id(tenant_main_graph_id(), "Person")
            .is_err()
    );

    let edge_only = PhysicalPlan::from_ops(vec![PlanOp::InsertEdge {
        variable: Some("e".into()),
        src: "a".into(),
        dst: "b".into(),
        direction: EdgeDirection::PointingRight,
        labels: vec!["Person".into()],
        properties: vec![],
    }]);

    let resolved = store
        .resolve_plan_labels(tenant_main_graph_id(), &[edge_only])
        .expect("resolve edge DML labels");
    assert_eq!(resolved.edge.len(), 1);
    assert_eq!(resolved.edge[0].name, "Person");
    assert_eq!(resolved.edge[0].id.raw(), 1);
    assert_eq!(
        store
            .lookup_vertex_label_id(tenant_main_graph_id(), "Person")
            .unwrap()
            .raw(),
        1
    );
    assert_eq!(
        store
            .lookup_edge_label_id(tenant_main_graph_id(), "Person")
            .unwrap()
            .raw(),
        1
    );
}

#[test]
fn resolve_plan_attaches_edge_payload_profile() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);

    register_test_graph(&store, admin, "tenant.main");

    let profile = EdgePayloadProfile {
        byte_width: 2,
        encoding: EdgePayloadEncoding::WeightRawU16,
    };
    store
        .admin_intern_edge_label(admin, "tenant.main", "KNOWS")
        .expect("intern edge");
    store
        .admin_set_edge_label_payload_profile(admin, "tenant.main", "KNOWS", profile.clone())
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
        .resolve_plan_labels(tenant_main_graph_id(), &[edge_only])
        .expect("resolve edge DML labels");
    assert_eq!(resolved.edge.len(), 1);
    assert_eq!(resolved.edge[0].name, "KNOWS");
    assert_eq!(resolved.edge[0].payload_profile, profile);
}

#[test]
fn label_stats_delta_updates_namespace_separated_stats() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);

    register_test_graph(&store, admin, "tenant.main");

    let vertex_label = store
        .admin_intern_vertex_label(admin, "tenant.main", "Person")
        .expect("vertex label");
    let edge_label = store
        .admin_intern_edge_label(admin, "tenant.main", "Person")
        .expect("edge label");

    store.apply_label_stats_delta_payload(
        tenant_main_graph_id(),
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(vertex_label, 2)],
            edge: vec![(edge_label, 3)],
        },
    );

    assert_eq!(
        store.vertex_label_stats(tenant_main_graph_id(), vertex_label),
        LabelStats {
            live_count: 2,
            total_adds: 2,
            total_removes: 0
        }
    );
    assert_eq!(
        store.edge_label_stats(tenant_main_graph_id(), edge_label),
        LabelStats {
            live_count: 3,
            total_adds: 3,
            total_removes: 0
        }
    );
    assert_eq!(
        store.vertex_label_shard_live_count(tenant_main_graph_id(), ShardId::new(0), vertex_label),
        2
    );
    assert_eq!(
        store.edge_label_shard_live_count(tenant_main_graph_id(), ShardId::new(0), edge_label),
        3
    );

    store.apply_label_stats_delta_payload(
        tenant_main_graph_id(),
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(vertex_label, -1)],
            edge: vec![(edge_label, -2)],
        },
    );

    assert_eq!(
        store.vertex_label_stats(tenant_main_graph_id(), vertex_label),
        LabelStats {
            live_count: 1,
            total_adds: 2,
            total_removes: 1
        }
    );
    assert_eq!(
        store.edge_label_stats(tenant_main_graph_id(), edge_label),
        LabelStats {
            live_count: 1,
            total_adds: 3,
            total_removes: 2
        }
    );
    assert_eq!(
        store.vertex_label_shard_live_count(tenant_main_graph_id(), ShardId::new(0), vertex_label),
        1
    );
    assert_eq!(
        store.edge_label_shard_live_count(tenant_main_graph_id(), ShardId::new(0), edge_label),
        1
    );
}

#[test]
fn label_stats_delta_tracks_per_shard_live_counts() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let label = store
        .admin_intern_vertex_label(admin, "tenant.main", "Person")
        .expect("vertex label");

    store.apply_label_stats_delta_payload(
        tenant_main_graph_id(),
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(label, 2)],
            edge: vec![],
        },
    );
    store.apply_label_stats_delta_payload(
        tenant_main_graph_id(),
        ShardId::new(1),
        &LabelStatsDelta {
            vertex: vec![(label, 1)],
            edge: vec![],
        },
    );
    store.apply_label_stats_delta_payload(
        tenant_main_graph_id(),
        ShardId::new(0),
        &LabelStatsDelta {
            vertex: vec![(label, -1)],
            edge: vec![],
        },
    );

    assert_eq!(
        store.vertex_label_stats(tenant_main_graph_id(), label),
        LabelStats {
            live_count: 2,
            total_adds: 3,
            total_removes: 1
        }
    );
    assert_eq!(
        store.vertex_label_shard_live_count(tenant_main_graph_id(), ShardId::new(0), label),
        1
    );
    assert_eq!(
        store.vertex_label_shard_live_count(tenant_main_graph_id(), ShardId::new(1), label),
        1
    );
}

#[test]
fn label_stats_projection_applies_delta_once_per_seq() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let label = store
        .admin_intern_vertex_label(admin, "tenant.main", "Person")
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
        tenant_main_graph_id(),
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
        store.vertex_label_stats(tenant_main_graph_id(), label),
        LabelStats {
            live_count: 2,
            total_adds: 2,
            total_removes: 0
        }
    );

    futures::executor::block_on(store.advance_label_stats_projection(
        tenant_main_graph_id(),
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
        store.vertex_label_stats(tenant_main_graph_id(), label),
        LabelStats {
            live_count: 2,
            total_adds: 2,
            total_removes: 0
        }
    );

    futures::executor::block_on(store.advance_label_stats_projection(
        tenant_main_graph_id(),
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
        store.vertex_label_stats(tenant_main_graph_id(), label),
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
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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

fn insert_mutation_record(
    caller: Principal,
    client_key: &str,
    created_at_ns: u64,
    routing_in_progress: bool,
) -> ClientMutationKey {
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), client_key.into());
    let mut record = RouterMutationRecord::new(1, created_at_ns, b"fp".to_vec());
    record.routing_in_progress = routing_in_progress;
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
        m.insert(key.clone(), record);
    });
    key
}

fn mutation_journal_len() -> u64 {
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.len())
}

fn shard_with(shard_id: u32, completed: bool, projection_advanced: bool) -> RouterMutationShard {
    let mut shard = RouterMutationShard::new(ShardId::new(shard_id), graph_principal(9), None);
    shard.completed = completed;
    shard.projection_advanced = projection_advanced;
    shard
}

fn insert_mutation_record_with_shards(
    caller: Principal,
    client_key: &str,
    created_at_ns: u64,
    shards: Vec<RouterMutationShard>,
) -> ClientMutationKey {
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), client_key.into());
    let mut record = RouterMutationRecord::new(1, created_at_ns, b"fp".to_vec());
    record.routing_in_progress = false;
    record.shards = shards;
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
        m.insert(key.clone(), record);
    });
    key
}

// ADR 0029 Phase 4: TTL eviction must retain non-terminal sagas (recovery targets) and only
// reclaim terminal ones; the old "not routing" rule wrongly stranded committed-but-unprojected
// federated mutations.
#[test]
fn ttl_eviction_retains_nonterminal_saga_but_evicts_terminal() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
    // Non-terminal: canonical committed on its shard, projection not yet advanced.
    let pending = insert_mutation_record_with_shards(
        graph_principal(1),
        "pending",
        0,
        vec![shard_with(0, true, false)],
    );
    // Terminal: routing released without an envelope and no canonical write -> Failed.
    let failed = insert_mutation_record(graph_principal(2), "failed", 0, false);

    let result = store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100, now)
        .expect("sweep");
    assert_eq!(result.removed, 1, "only the terminal saga is evicted");
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
        assert!(
            m.get(&pending).is_some(),
            "non-terminal saga must be retained as a recovery target"
        );
        assert!(m.get(&failed).is_none(), "terminal saga must be evicted");
    });
}

// ADR 0029 Phase 4: an unexpired routing lease blocks a concurrent owner, but a lease past
// ROUTING_LEASE_TTL_NS is reclaimable (the previous owner trapped before persisting an
// envelope, so no canonical write happened and reclaiming is safe).
#[test]
fn expired_routing_lease_is_reclaimable_by_retry() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);

    let first = store
        .reserve_mutation_id_for_client_key_at(
            caller,
            tenant_main_graph_id(),
            "k",
            b"a".to_vec(),
            0,
        )
        .expect("first owner");
    assert!(first.routing_owner);

    assert_eq!(
        store.reserve_mutation_id_for_client_key_at(
            caller,
            tenant_main_graph_id(),
            "k",
            b"a".to_vec(),
            ROUTING_LEASE_TTL_NS,
        ),
        Err(RouterError::Conflict(
            "client_mutation_key is already in progress; retry later".into()
        )),
        "an unexpired routing lease must block a concurrent owner"
    );

    let reclaimed = store
        .reserve_mutation_id_for_client_key_at(
            caller,
            tenant_main_graph_id(),
            "k",
            b"a".to_vec(),
            ROUTING_LEASE_TTL_NS + 1,
        )
        .expect("reclaim expired lease");
    assert_eq!(reclaimed.mutation_id, first.mutation_id);
    assert!(reclaimed.routing_owner);
}

// ADR 0030 slice 6: a terminally-failed mutation is irreversible — a same-key retry must return
// the stored terminal error verbatim instead of re-routing. Try runs after the dispatch envelope is
// recorded, so the cancelable state is "envelope present + no committed canonical shard", and only a
// new client key may attempt the work again.
#[test]
fn terminally_failed_mutation_blocks_same_key_retry() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(7);

    // Uncommitted dispatch: a durable envelope exists but no shard's canonical write committed.
    // `insert_mutation_record_with_shards` uses mutation_id 1 and `fp` as the fingerprint.
    let key = insert_mutation_record_with_shards(caller, "k", 0, vec![shard_with(0, false, false)]);
    assert!(
        store.terminally_fail_uncommitted_dispatch(&key, 1, "uniqueness reclaim cancelled".into()),
        "an uncommitted-dispatch record can be terminal-ized"
    );
    // Idempotent: a sibling reservation of the same already-failed mutation is still cancelable.
    assert!(
        store.terminally_fail_uncommitted_dispatch(&key, 1, "other".into()),
        "already-terminal mutation reports cancelable (idempotent)"
    );

    // Same key + same fingerprint, within TTL: the stored terminal error is returned, not a retry.
    assert_eq!(
        store.reserve_mutation_id_for_client_key_at(
            caller,
            tenant_main_graph_id(),
            "k",
            b"fp".to_vec(),
            1,
        ),
        Err(RouterError::Conflict("uniqueness reclaim cancelled".into())),
        "a terminally-failed mutation must not be re-dispatched under the same key"
    );
}

// ADR 0030 slice 6: terminal-ization is fenced on the uncommitted-dispatch predicate + mutation_id.
// A re-routed (`Routing`) mutation, one whose canonical write completed on a shard, an id mismatch
// (recycled key), or a missing record must all be refused so the reconciler holds.
#[test]
fn terminal_failure_is_refused_unless_uncommitted_dispatch() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    // Routing: a retry reclaimed the key and is re-dispatching.
    let routing = insert_mutation_record(graph_principal(1), "routing", 0, true);
    assert!(
        !store.terminally_fail_uncommitted_dispatch(&routing, 1, "late cancel".into()),
        "a Routing mutation must not be terminal-ized"
    );

    // Canonical-committed: a shard's canonical write committed, so the value may be live.
    let committed = insert_mutation_record_with_shards(
        graph_principal(2),
        "committed",
        0,
        vec![shard_with(0, true, false)],
    );
    assert!(
        !store.terminally_fail_uncommitted_dispatch(&committed, 1, "late cancel".into()),
        "a mutation with a completed canonical shard must not be terminal-ized"
    );

    // Mutation-id mismatch: a recycled client key must not be terminal-ized by another mutation.
    let uncommitted = insert_mutation_record_with_shards(
        graph_principal(3),
        "uncommitted",
        0,
        vec![shard_with(0, false, false)],
    );
    assert!(
        !store.terminally_fail_uncommitted_dispatch(&uncommitted, 999, "wrong id".into()),
        "an id mismatch must be refused"
    );
    assert!(
        store.terminally_fail_uncommitted_dispatch(&uncommitted, 1, "right id".into()),
        "the matching id is eligible"
    );

    // Missing record: nothing to flip.
    let missing = ClientMutationKey::new(graph_principal(4), tenant_main_graph_id(), "gone".into());
    assert!(!store.terminally_fail_uncommitted_dispatch(&missing, 1, "x".into()));
}

// ADR 0030 slice 6: the reverse index counts a mutation's non-terminal reservations — fresh inserts
// at Try bump it (creating the row, pinned to the owning client key), each FreshlyCommitted/Cancel
// release decrements, and the row (and pin) only vanishes at zero. A unique high mutation_id keeps
// the global-keyed reverse index isolated from records using the default test id.
#[test]
fn reservation_slot_count_increments_and_releases_to_zero() {
    let store = RouterStore::new();
    let caller = graph_principal(71);
    let mid: u64 = 9_100_001;
    let key = ClientMutationKey::new(caller, GraphId::from_raw(71), "rk-count".into());

    // A fresh count of zero (a pure idempotent replay) creates no row and pins nothing.
    store.apply_reservation_slots(mid, &key, 0);
    assert!(store.reservation_index_client_key(mid).is_none());

    // Two fresh inserts then one more accumulate to three, pinned to `key`.
    store.apply_reservation_slots(mid, &key, 2);
    store.apply_reservation_slots(mid, &key, 1);
    assert_eq!(store.reservation_index_client_key(mid), Some(key.clone()));

    // The row survives until the third release drains the count to zero.
    store.release_reservation_slot(mid);
    store.release_reservation_slot(mid);
    assert_eq!(store.reservation_index_client_key(mid), Some(key));
    store.release_reservation_slot(mid);
    assert!(store.reservation_index_client_key(mid).is_none());
}

// ADR 0030 slice 6: the GC pin is fail-closed. A release with no counted reservation is an
// under-count that could un-pin a still-referenced record, so it traps (rolling back the offending
// Confirm/Cancel) rather than masking the inconsistency with a no-op.
#[test]
#[should_panic(expected = "no reverse index row")]
fn reservation_slot_release_without_count_traps() {
    let store = RouterStore::new();
    store.release_reservation_slot(9_150_001);
}

// ADR 0030 slice 6: a `mutation_id` maps to exactly one client key; a bump that finds a row owned by
// a different key is corruption and must trap, not silently re-pin under the wrong owner.
#[test]
#[should_panic(expected = "owned by a different client key")]
fn reservation_slot_apply_rejects_mismatched_owner() {
    let store = RouterStore::new();
    let mid: u64 = 9_160_001;
    let first = ClientMutationKey::new(graph_principal(74), GraphId::from_raw(74), "rk-a".into());
    let second = ClientMutationKey::new(graph_principal(75), GraphId::from_raw(75), "rk-b".into());
    store.apply_reservation_slots(mid, &first, 1);
    store.apply_reservation_slots(mid, &second, 1);
}

// ADR 0030 slice 6: the count-overflow guard is a read-only preflight that runs before any
// reservation is written, so a Try that would overflow `u32` is rejected with nothing mutated.
#[test]
fn reservation_slot_overflow_preflight_rejects_before_apply() {
    let store = RouterStore::new();
    let caller = graph_principal(72);
    let mid: u64 = 9_200_001;
    let key = ClientMutationKey::new(caller, GraphId::from_raw(72), "rk-of".into());

    // Drive the count to one below the ceiling (apply saturates; the test needs no preflight).
    store.apply_reservation_slots(mid, &key, u32::MAX - 1);

    // One more fits exactly; five would overflow and is refused.
    assert!(store.preflight_reservation_slots(mid, 1).is_ok());
    assert!(matches!(
        store.preflight_reservation_slots(mid, 5),
        Err(RouterError::Internal(_))
    ));
    // The refused preflight mutated nothing: the owning key is unchanged.
    assert_eq!(store.reservation_index_client_key(mid), Some(key));
}

// ADR 0030 slice 6: a terminal, past-TTL record stays GC-pinned while it still owns a non-terminal
// reservation (the reclaim reconciler needs it for a terminal-failure decision), and is evicted —
// along with its reverse row — only once that last reservation is released.
#[test]
fn gc_pin_retains_terminal_record_until_reservation_released() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
    let caller = graph_principal(73);
    let mid: u64 = 9_300_001;
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), "rk-gc".into());

    // Terminal (Failed: no envelope, no canonical write) and well past the TTL window.
    let mut record = RouterMutationRecord::new(mid, 0, b"fp".to_vec());
    record.routing_in_progress = false;
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.insert(key.clone(), record));
    assert!(
        store
            .router_mutation_record(caller, tenant_main_graph_id(), "rk-gc")
            .expect("record")
            .is_terminal()
    );

    // Pin it: a non-terminal reservation still depends on this record.
    store.apply_reservation_slots(mid, &key, 1);
    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100_000, now)
        .expect("sweep");
    assert!(
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key).is_some()),
        "a pinned terminal record must survive TTL GC"
    );
    assert!(store.reservation_index_client_key(mid).is_some());

    // Release the last reservation; the reverse row goes immediately, and GC may now reclaim the
    // record (and idempotently the reverse row) together.
    store.release_reservation_slot(mid);
    assert!(store.reservation_index_client_key(mid).is_none());
    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100_000, now)
        .expect("sweep");
    assert!(
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key).is_none()),
        "an unpinned terminal record past TTL must be evicted"
    );
    assert!(store.reservation_index_client_key(mid).is_none());
}

// ADR 0030 slice 6: a terminal, past-TTL record with no reservation (e.g. a constrained DELETE's
// Release, or an orphan) still stays GC-pinned while a pending unique-effect discovery row remains —
// Driver 2 reads this record's completion state before it removes the row — and is evicted only once
// the last pending row is gone.
#[test]
fn gc_pin_retains_terminal_record_until_pending_effect_removed() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
    let caller = graph_principal(74);
    let mid: u64 = 9_300_101;
    let shard = gleaph_graph_kernel::federation::ShardId::new(0);
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), "rk-pe".into());

    // Terminal (Failed: no envelope, no canonical write) and well past the TTL window.
    let mut record = RouterMutationRecord::new(mid, 0, b"fp".to_vec());
    record.routing_in_progress = false;
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.insert(key.clone(), record));

    // Pin it via a pending-effect row (no reservation reverse-row exists for this mutation).
    store.register_pending_unique_effect(tenant_main_graph_id(), mid, shard, caller, key.clone());
    assert!(store.reservation_index_client_key(mid).is_none());
    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100_000, now)
        .expect("sweep");
    assert!(
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key).is_some()),
        "a terminal record with a pending-effect row must survive TTL GC"
    );

    // Remove the last pending row; the record may now be reclaimed.
    crate::facade::stable::unique_effect_pending::remove(tenant_main_graph_id(), mid, shard);
    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100_000, now)
        .expect("sweep");
    assert!(
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key).is_none()),
        "a terminal record past TTL with no pending row must be evicted"
    );
}

// ADR 0029 Phase 4: the recovery driver's scan returns only sagas it can safely converge —
// non-terminal records that already have a persisted dispatch envelope (shards) and are not
// held by an active routing lease.
#[test]
fn scan_recoverable_selects_only_nonterminal_with_envelope() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    insert_mutation_record(graph_principal(1), "routing", 0, true);
    insert_mutation_record(graph_principal(2), "failed", 0, false);
    let pending = insert_mutation_record_with_shards(
        graph_principal(3),
        "pending",
        0,
        vec![shard_with(0, true, false)],
    );
    insert_mutation_record_with_shards(
        graph_principal(4),
        "done",
        0,
        vec![shard_with(0, true, true)],
    );

    let (keys, _last, scanned) = store.scan_recoverable_mutations(None, 100);
    assert_eq!(scanned, 4);
    assert_eq!(
        keys.len(),
        1,
        "only the committed-but-unprojected saga is recoverable"
    );
    assert!(keys.contains(&pending));
}

// ADR 0029 Phase 4: recovery diagnostics attach to live sagas and never resurrect a terminal
// record.
#[test]
fn record_last_error_sets_diagnostic_and_skips_terminal() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let pending = insert_mutation_record_with_shards(
        graph_principal(1),
        "pending",
        0,
        vec![shard_with(0, true, false)],
    );
    store
        .record_router_mutation_last_error(&pending, "boom".into())
        .expect("record diagnostic");
    let live = ROUTER_MUTATION_BY_CLIENT_KEY
        .with_borrow(|m| m.get(&pending))
        .expect("record");
    assert_eq!(live.last_error.as_deref(), Some("boom"));

    let failed = insert_mutation_record(graph_principal(2), "failed", 0, false);
    store
        .record_router_mutation_last_error(&failed, "ignored".into())
        .expect("no-op on terminal");
    let terminal = ROUTER_MUTATION_BY_CLIENT_KEY
        .with_borrow(|m| m.get(&failed))
        .expect("record");
    assert!(
        terminal.last_error.is_none(),
        "a terminal record must not record a recovery diagnostic"
    );
}

#[test]
fn sweep_removes_expired_but_keeps_fresh_and_in_progress() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
    let expired = insert_mutation_record(graph_principal(1), "expired", 0, false);
    let fresh = insert_mutation_record(graph_principal(2), "fresh", now, false);
    // Expired in wall-clock terms but still actively routing: must not be yanked.
    let expired_in_progress = insert_mutation_record(graph_principal(3), "stuck", 0, true);
    assert_eq!(mutation_journal_len(), 3);

    let result = store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100, now)
        .expect("sweep");
    assert_eq!(result.scanned, 3);
    assert_eq!(result.removed, 1);
    assert!(result.done);
    assert!(result.next_cursor.is_none());

    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
        assert!(m.get(&expired).is_none(), "expired record must be evicted");
        assert!(
            m.get(&fresh).is_some(),
            "within-TTL record must be retained"
        );
        assert!(
            m.get(&expired_in_progress).is_some(),
            "in-progress reservation must never be yanked"
        );
    });
}

#[test]
fn sweep_paginates_with_cursor_until_done() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
    for i in 0..5u8 {
        insert_mutation_record(graph_principal(100 + i), "expired", 0, false);
    }
    assert_eq!(mutation_journal_len(), 5);

    // Budgeted slices must collectively scan the whole keyspace and evict all
    // expired records (bounded growth guarantee).
    let mut cursor = None;
    let mut total_scanned = 0u32;
    let mut total_removed = 0u32;
    loop {
        let result = store
            .admin_sweep_expired_client_mutation_keys_at(admin, cursor.clone(), 2, now)
            .expect("sweep step");
        total_scanned += result.scanned;
        total_removed += result.removed;
        if result.done {
            break;
        }
        cursor = result.next_cursor;
        assert!(cursor.is_some(), "non-done step must yield a resume cursor");
    }
    assert_eq!(total_scanned, 5);
    assert_eq!(total_removed, 5);
    assert_eq!(mutation_journal_len(), 0, "journal must be fully bounded");
}

#[test]
fn sweep_requires_admin_and_nonzero_budget() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let non_admin = graph_principal(7);
    assert_eq!(
        store.admin_sweep_expired_client_mutation_keys_at(non_admin, None, 10, 0),
        Err(RouterError::NotAuthorized)
    );
    assert_eq!(
        store.admin_sweep_expired_client_mutation_keys_at(admin, None, 0, 0),
        Err(RouterError::InvalidArgument(
            "max_scan must be greater than zero".into()
        ))
    );
}

#[test]
fn client_mutation_key_blocks_concurrent_routing_owner() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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
    let admin = Principal::from_slice(&[1; 29]);
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

    // ADR 0025 (E): once fully completed + projected, the heavy fields are dropped and
    // the final row count is pinned, so replay still returns Some(5) from a small record.
    let compacted = store
        .router_mutation_record(caller, tenant_main_graph_id(), "client-key-1")
        .expect("record");
    assert_eq!(compacted.completed_row_count, Some(5));
    assert!(compacted.shards.is_empty(), "shard fan-out must be dropped");
    assert!(
        compacted.resolved_labels.is_none() && compacted.resolved_properties.is_none(),
        "resolved tables must be dropped after completion"
    );
}

#[test]
fn amortized_gc_evicts_expired_and_keeps_fresh() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    crate::facade::store::idempotency::reset_mutation_gc_cursor_for_test();

    let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
    for i in 0..5u8 {
        insert_mutation_record(graph_principal(50 + i), "old", 0, false);
    }
    let fresh = insert_mutation_record(graph_principal(200), "fresh", now, false);
    assert_eq!(mutation_journal_len(), 6);

    // The heap round-robin cursor laps the keyspace; a bounded number of GC steps
    // (budget 2 each) must evict every expired record while retaining the fresh one.
    for _ in 0..8 {
        store.gc_expired_client_mutation_keys(now);
    }
    assert_eq!(
        mutation_journal_len(),
        1,
        "all expired records must be evicted by amortized GC"
    );
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
        assert!(
            m.get(&fresh).is_some(),
            "within-TTL record must be retained"
        );
    });
}

#[test]
fn router_mutation_journal_records_zero_shard_completion() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
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

mod graph_type_catalog_vocabulary {
    use super::*;
    use crate::facade::stable::graph_type_catalog::apply_catalog_statement_block;
    use gleaph_gql::ast::StatementBlock;
    use gleaph_gql::parser;

    const PERSON_KNOWS: &str = "NODE Person LABEL Person { name STRING }, DIRECTED EDGE KNOWS LABEL KNOWS CONNECTING (Person -> Person)";

    fn catalog_block_from(gql: &str) -> StatementBlock {
        parser::parse(gql)
            .expect("parse")
            .transaction_activity
            .expect("tx")
            .body
            .expect("body")
    }

    #[test]
    fn create_graph_inline_ddl_auto_interns_vocabulary() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        let ddl = format!("CREATE GRAPH g {{ {PERSON_KNOWS} }}");
        apply_catalog_statement_block(&catalog_block_from(&ddl)).expect("apply ddl");

        assert!(store.lookup_vertex_label_id(graph_id, "Person").is_ok());
        assert!(store.lookup_edge_label_id(graph_id, "KNOWS").is_ok());
        assert!(store.lookup_property_id(graph_id, "name").is_ok());
    }

    #[test]
    fn create_graph_typed_ddl_auto_interns_vocabulary() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        let ddl = format!("CREATE GRAPH TYPE gt {{ {PERSON_KNOWS} }} NEXT CREATE GRAPH g TYPED gt");
        apply_catalog_statement_block(&catalog_block_from(&ddl)).expect("apply ddl");

        assert!(store.lookup_vertex_label_id(graph_id, "Person").is_ok());
        assert!(store.lookup_edge_label_id(graph_id, "KNOWS").is_ok());
        assert!(store.lookup_property_id(graph_id, "name").is_ok());
    }

    #[test]
    fn create_graph_any_skips_vocabulary_intern() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        apply_catalog_statement_block(&catalog_block_from("CREATE GRAPH g ANY"))
            .expect("apply ddl");

        assert!(matches!(
            store.lookup_vertex_label_id(graph_id, "Person"),
            Err(RouterError::NotFound(_))
        ));
    }

    // --- ADR 0031 Slice 4: vector dispatch activation + per-graph readiness ---

    fn setup_one_shard_graph(store: &RouterStore, admin: Principal) -> GraphId {
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(store, admin, "tenant.main");
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: graph_principal(1),
                index_canister: graph_principal(2),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register shard 0");
        tenant_main_graph_id()
    }

    /// Register a vector-index def for `tenant.main` pointing at `target` so the readiness predicate
    /// has a resolved graph target to match shard attachments against (ADR 0031 Slice 4).
    fn register_vector_def(graph_id: GraphId, index_id: u32, target: Principal) {
        crate::facade::stable::vector_index_catalog::register_vector_index(
            graph_id,
            index_id,
            gleaph_graph_kernel::entry::EmbeddingNameId::from_raw(index_id as u16),
            gleaph_graph_kernel::vector_index::VectorIndexKind::IvfFlat,
            gleaph_graph_kernel::vector_index::VectorMetric::L2Squared,
            gleaph_graph_kernel::vector_index::VectorEncoding::F32,
            16,
            Some(
                crate::facade::stable::vector_index_catalog::VectorIndexTarget { canister: target },
            ),
            false,
        )
        .expect("register vector index def");
    }

    #[test]
    fn vector_dispatch_not_ready_until_flag_and_attach() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        let graph_id = setup_one_shard_graph(&store, admin);
        register_vector_def(graph_id, 1, graph_principal(7));

        // Default: global flag off, no vector attach.
        assert!(!store.graph_vector_dispatch_ready(graph_id));

        // Flag on alone is not enough — the shard is not vector-attached yet.
        crate::facade::stable::vector_activation::set_vector_dispatch_globally_enabled(true);
        assert!(
            !store.graph_vector_dispatch_ready(graph_id),
            "global flag alone must not enable dispatch while shards are unattached"
        );

        // Attach the shard's vector target; now both conditions hold.
        futures::executor::block_on(store.admin_attach_vector_index_shard(
            admin,
            AdminAttachVectorIndexShardArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                vector_index_canister: graph_principal(7),
            },
        ))
        .expect("attach vector index shard");
        assert!(store.graph_vector_dispatch_ready(graph_id));

        // A shard attached to a canister that is *not* the def target must not be ready (the
        // misrouting hole): point the def at a different canister and readiness drops.
        crate::facade::stable::vector_index_catalog::purge_graph_vector_indexes(graph_id);
        register_vector_def(graph_id, 2, graph_principal(8));
        assert!(
            !store.graph_vector_dispatch_ready(graph_id),
            "shard attached to a non-target canister must not satisfy readiness"
        );
        // Realign the def target with the shard's attachment and readiness returns.
        crate::facade::stable::vector_index_catalog::purge_graph_vector_indexes(graph_id);
        register_vector_def(graph_id, 3, graph_principal(7));
        assert!(store.graph_vector_dispatch_ready(graph_id));

        // Flipping the flag back off re-closes the gate (reversible).
        crate::facade::stable::vector_activation::set_vector_dispatch_globally_enabled(false);
        assert!(!store.graph_vector_dispatch_ready(graph_id));
    }

    #[test]
    fn vector_attach_is_idempotent_and_enforces_one_target_per_graph() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        let _graph_id = setup_one_shard_graph(&store, admin);

        let attach = |target: Principal| {
            futures::executor::block_on(store.admin_attach_vector_index_shard(
                admin,
                AdminAttachVectorIndexShardArgs {
                    logical_graph_name: "tenant.main".into(),
                    shard_id: ShardId::new(0),
                    vector_index_canister: target,
                },
            ))
        };

        attach(graph_principal(7)).expect("first attach");
        // Idempotent replay to the same target is a no-op.
        attach(graph_principal(7)).expect("idempotent re-attach");

        // Register a second shard and try to point it at a *different* vector canister.
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(1),
                graph_canister: graph_principal(3),
                index_canister: graph_principal(2),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register shard 1");
        let err = futures::executor::block_on(store.admin_attach_vector_index_shard(
            admin,
            AdminAttachVectorIndexShardArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(1),
                vector_index_canister: graph_principal(9),
            },
        ))
        .expect_err("conflicting target must be rejected");
        assert!(
            matches!(err, RouterError::Conflict(_)),
            "one vector-index target per graph, got {err:?}"
        );
    }

    #[test]
    fn vector_attach_rejects_anonymous_target() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        let _graph_id = setup_one_shard_graph(&store, admin);
        let err = futures::executor::block_on(store.admin_attach_vector_index_shard(
            admin,
            AdminAttachVectorIndexShardArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                vector_index_canister: Principal::anonymous(),
            },
        ))
        .expect_err("anonymous target rejected");
        assert!(matches!(err, RouterError::InvalidArgument(_)), "{err:?}");
    }
}

mod uniqueness_constraints {
    use crate::facade::stable::constraint_catalog::find_active_unique_constraint;
    use crate::facade::stable::constraint_name_catalog::lookup_constraint_name_id;
    use crate::facade::store::RouterStore;
    use crate::facade::store::catalog_test_support::{GRAPH, setup};
    use crate::state::RouterError;

    #[test]
    fn create_on_unused_label_succeeds_and_interns() {
        let (store, _admin, graph_id) = setup();
        // A read-only lookup must not intern the label.
        assert!(store.lookup_vertex_label_id(graph_id, "User").is_err());
        assert!(store.lookup_vertex_label_id(graph_id, "User").is_err());

        store
            .create_unique_constraint(graph_id, "user_email", false, "User", "email")
            .expect("create constraint");

        let label_id = store
            .lookup_vertex_label_id(graph_id, "User")
            .expect("label interned by CREATE");
        let property_id = store
            .lookup_property_id(graph_id, "email")
            .expect("property interned by CREATE");
        let name_id = lookup_constraint_name_id(graph_id, "user_email").expect("name interned");
        let (found_name, def) = find_active_unique_constraint(graph_id, label_id, property_id)
            .expect("constraint registered");
        assert_eq!(found_name, name_id);
        assert_eq!(def.vertex_label_id, label_id);
        assert_eq!(def.property_id, property_id);
    }

    #[test]
    fn existing_label_is_rejected_even_at_zero_live_count() {
        let (store, admin, graph_id) = setup();
        // An admin-interned label has zero live elements but must still be rejected.
        store
            .admin_intern_vertex_label(admin, GRAPH, "User")
            .expect("intern label");
        let err = store
            .create_unique_constraint(graph_id, "user_email", false, "User", "email")
            .unwrap_err();
        assert!(matches!(err, RouterError::Conflict(_)));
        assert!(lookup_constraint_name_id(graph_id, "user_email").is_none());
    }

    #[test]
    fn graph_type_interned_label_is_rejected() {
        let (store, _admin, graph_id) = setup();
        // Graph-type / catalog vocabulary interning uses this same commit path.
        RouterStore::commit_intern_vertex_label_name(graph_id, "User").expect("intern label");
        let err = store
            .create_unique_constraint(graph_id, "user_email", false, "User", "email")
            .unwrap_err();
        assert!(matches!(err, RouterError::Conflict(_)));
    }

    #[test]
    fn read_only_lookup_does_not_intern_label() {
        let (store, _admin, graph_id) = setup();
        assert!(store.lookup_vertex_label_id(graph_id, "Ghost").is_err());
        // Still absent → CREATE on it succeeds (would fail if the lookup had interned it).
        store
            .create_unique_constraint(graph_id, "ghost_key", false, "Ghost", "k")
            .expect("create constraint on still-unused label");
    }

    #[test]
    fn failed_create_leaves_no_partial_state() {
        let (store, _admin, graph_id) = setup();
        store
            .create_unique_constraint(graph_id, "dup", false, "Account", "handle")
            .expect("create first");
        // Reusing the same constraint name on a different brand-new label must fail in the
        // read-only preflight (name already defined) before any catalog mutation.
        let err = store
            .create_unique_constraint(graph_id, "dup", false, "Member", "code")
            .unwrap_err();
        assert!(matches!(err, RouterError::Conflict(_)));
        // The would-be new label and property were never interned, and no constraint exists.
        assert!(store.lookup_vertex_label_id(graph_id, "Member").is_err());
        assert!(store.lookup_property_id(graph_id, "code").is_err());
    }

    #[test]
    fn drop_then_recreate_on_same_label_is_rejected() {
        let (store, _admin, graph_id) = setup();
        store
            .create_unique_constraint(graph_id, "c", false, "User", "email")
            .expect("create");
        store
            .begin_drop_unique_constraint(graph_id, "c", false)
            .expect("drop");
        // The label remains interned while the constraint is Dropping, so re-CREATE with a new name
        // on the same label is rejected by the declare-on-empty (brand-new-label) contract.
        let err = store
            .create_unique_constraint(graph_id, "c2", false, "User", "email")
            .unwrap_err();
        assert!(matches!(err, RouterError::Conflict(_)));
    }

    #[test]
    fn recreate_same_name_while_dropping_is_rejected() {
        let (store, _admin, graph_id) = setup();
        store
            .create_unique_constraint(graph_id, "c", false, "User", "email")
            .expect("create");
        store
            .begin_drop_unique_constraint(graph_id, "c", false)
            .expect("drop");
        // The same name is a tombstone while Dropping: re-CREATE (even on a brand-new label, even
        // with IF NOT EXISTS) is rejected transiently until the drain completes (Removed).
        let err = store
            .create_unique_constraint(graph_id, "c", false, "Member", "code")
            .unwrap_err();
        assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
        let err = store
            .create_unique_constraint(graph_id, "c", true, "Member", "code")
            .unwrap_err();
        assert!(
            matches!(err, RouterError::Conflict(_)),
            "IF NOT EXISTS does not bypass the tombstone, got {err:?}"
        );
    }
}
