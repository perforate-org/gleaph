use super::super::stable::bulk_load::{
    BulkLoadChunkEnvelopeV1, BulkLoadChunkProgressV1, BulkLoadChunkReceiptKey,
    BulkLoadChunkReceiptRecordV1, BulkLoadGraphReceiptV1, BulkLoadGraphRequestV1,
};
use super::super::stable::label_stats::{
    BulkLoadCoordinatorV1, BulkLoadLifecycleV1, BulkLoadTargetV1,
};
use super::super::stable::label_stats::{
    ClientMutationKey, LabelStats, RouterMutationPayloadV1, RouterMutationRecord,
    RouterMutationShardV1,
};
use super::*;
use crate::facade::stable::{
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS, ROUTER_EDGE_INLINE_PROPERTY_PROFILES,
};
use crate::init::RouterInitArgs;
use crate::types::{
    AdminAttachVectorIndexShardArgs, AdminRegisterShardArgs, AtomicInsertVertexV1, BulkLoadChunkV1,
    GraphRegistryEntry, GraphStatus, ProvisioningState,
};
use candid::Principal;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::{NodeLabelRef, PhysicalPlan, PlanOp};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    GraphOrderedVertexBatchReceiptV1, LabelStatsDelta, LabelStatsDeltaEventWire,
    OrderedVertexBatchGraphItemV1, OrderedVertexBatchGraphRequestV1, ResolvedInlineSchema,
    ResolvedLabelTable, ResolvedPropertyTable,
};
use std::collections::BTreeSet;
use std::ops::Bound;

use crate::facade::stable::graph_catalog::lookup_graph_id;
#[cfg(test)]
use crate::facade::store::bulk_load::BULK_LOAD_RECEIPT_ROW_READS;
use crate::facade::store::registry_invariants::assert_registry_invariants;

pub(crate) fn graph_principal(byte: u8) -> Principal {
    Principal::self_authenticating([byte; 32])
}

pub(crate) fn test_init_args() -> RouterInitArgs {
    RouterInitArgs {
        issuing_principal: Principal::anonymous(),
        initial_admins: vec![],
        provision_canister: None,
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

fn test_mutation_key(caller: Principal, graph_id: GraphId, client_key: &str) -> ClientMutationKey {
    ClientMutationKey::new(caller, graph_id, client_key.to_owned())
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
    crate::facade::store::catalog_test_support::intern_property(
        &store,
        admin,
        "tenant.main",
        "age",
    );
    store
        .admin_set_edge_label_inline_property_profile(
            admin,
            "tenant.main",
            "KNOWS",
            EdgeInlinePropertyProfile {
                byte_width: 2,
                encoding: EdgeInlinePropertyEncoding::RawU16,
            },
        )
        .expect("inline property profile");
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
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            "tenant.main",
            "age",
        )
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
fn unregister_shard_with_vector_target_removes_vector_readiness_row() {
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

    let vector_canister = graph_principal(7);
    futures::executor::block_on(store.admin_attach_vector_index_shard(
        admin,
        AdminAttachVectorIndexShardArgs {
            logical_graph_name: "tenant.main".into(),
            shard_id: ShardId::new(0),
            vector_canister,
        },
    ))
    .expect("attach vector target");

    let graph_id = tenant_main_graph_id();
    ROUTER_SHARDS.with_borrow(|shards| {
        let entry = shards
            .get(&GraphShardKey::new(graph_id, ShardId::new(0)))
            .expect("shard row");
        assert_eq!(entry.vector_canister, Some(vector_canister));
        assert!(entry.vector_index_attached);
    });

    futures::executor::block_on(store.admin_unregister_shard(
        admin,
        "tenant.main",
        ShardId::new(0),
    ))
    .expect("unregister shard");

    assert!(store.resolve_shard(graph_id, ShardId::new(0)).is_err());
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
fn lookup_vertex_label_id_fails_closed_on_unknown_label() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    store
        .admin_intern_vertex_label(admin, "tenant.main", "Person")
        .expect("intern label");
    // Known label resolves; an unknown label fails closed (the `admin_register_vector_index`
    // label-resolution path relies on this to reject a registration with an unknown label).
    assert!(
        store
            .lookup_vertex_label_id(tenant_main_graph_id(), "Person")
            .is_ok()
    );
    assert!(matches!(
        store.lookup_vertex_label_id(tenant_main_graph_id(), "Unknown"),
        Err(RouterError::NotFound(_))
    ));
}

#[test]
fn resolve_vector_index_labels_rejects_empty_and_unknown() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    store
        .admin_intern_vertex_label(admin, "tenant.main", "Person")
        .expect("intern label");
    let graph_id = tenant_main_graph_id();

    // Known label resolves; unknown label fails closed; empty set is rejected (ADR 0064 §Router
    // catalog: the index is scoped to a non-empty creation-fixed label set).
    assert_eq!(
        crate::facade::stable::vector_index_catalog::resolve_vector_index_labels(
            &store,
            graph_id,
            &["Person".to_string()]
        )
        .expect("resolve known label"),
        vec![gleaph_graph_kernel::entry::VertexLabelId::from_raw(1)]
    );
    assert!(matches!(
        crate::facade::stable::vector_index_catalog::resolve_vector_index_labels(
            &store,
            graph_id,
            &["Unknown".to_string()]
        ),
        Err(RouterError::NotFound(_))
    ));
    assert!(matches!(
        crate::facade::stable::vector_index_catalog::resolve_vector_index_labels(
            &store,
            graph_id,
            &[]
        ),
        Err(RouterError::InvalidArgument(_))
    ));
}

#[test]
fn ordered_catalog_resolution_is_read_only_and_deduplicates_names() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let edge_id = store
        .admin_intern_edge_label(admin, "tenant.main", "KNOWS")
        .expect("edge label");
    let property_id = crate::facade::store::catalog_test_support::intern_property(
        &store,
        admin,
        "tenant.main",
        "weight",
    );

    let (labels, properties) = store
        .resolve_ordered_edge_catalogs(
            tenant_main_graph_id(),
            vec![Some("KNOWS".into()), Some("KNOWS".into()), None],
            vec!["weight".into(), "weight".into()],
        )
        .expect("resolve ordered catalogs");
    assert_eq!(labels.edge.len(), 1);
    assert_eq!(labels.edge[0].id, edge_id);
    assert_eq!(properties.properties.len(), 1);
    assert_eq!(properties.properties[0].id, property_id);
    assert!(
        store
            .lookup_edge_label_id(tenant_main_graph_id(), "MISSING")
            .is_err()
    );
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
fn resolve_plan_attaches_edge_inline_property_profile() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);

    register_test_graph(&store, admin, "tenant.main");

    let profile = EdgeInlinePropertyProfile {
        byte_width: 2,
        encoding: EdgeInlinePropertyEncoding::RawU16,
    };
    store
        .admin_intern_edge_label(admin, "tenant.main", "KNOWS")
        .expect("intern edge");
    store
        .admin_set_edge_label_inline_property_profile(
            admin,
            "tenant.main",
            "KNOWS",
            profile.clone(),
        )
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
    assert_eq!(resolved.edge[0].inline_property_profile, profile);
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
    let key = test_mutation_key(caller, tenant_main_graph_id(), "client-key-1");

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
            &key,
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            vec![RouterMutationShardV1::new(
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
        let mut record = RouterMutationRecord::new(
            1,
            0u64.saturating_sub(CLIENT_MUTATION_KEY_TTL_NS + 1),
            b"a".to_vec(),
        );
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().completed_row_count = Some(0);
        record.mark_terminal_at_ns(0u64.saturating_sub(CLIENT_MUTATION_KEY_TTL_NS + 1));
        m.insert(key, record);
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
    record.as_v1_mut().routing_in_progress = routing_in_progress;
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
        m.insert(key.clone(), record);
    });
    key
}

fn insert_completed_mutation_record(
    caller: Principal,
    client_key: &str,
    terminal_at_ns: u64,
) -> ClientMutationKey {
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), client_key.into());
    let mut record = RouterMutationRecord::new(1, terminal_at_ns, b"fp".to_vec());
    record.as_v1_mut().routing_in_progress = false;
    record.as_v1_mut().completed_row_count = Some(0);
    record.mark_terminal_at_ns(terminal_at_ns);
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
        m.insert(key.clone(), record);
    });
    key
}

fn mutation_journal_len() -> u64 {
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.len())
}

fn shard_with(shard_id: u32, completed: bool, projection_advanced: bool) -> RouterMutationShardV1 {
    let mut shard = RouterMutationShardV1::new(ShardId::new(shard_id), graph_principal(9), None);
    shard.set_completed(completed);
    shard.set_projection_advanced(projection_advanced);
    shard
}

fn insert_mutation_record_with_shards(
    caller: Principal,
    client_key: &str,
    created_at_ns: u64,
    shards: Vec<RouterMutationShardV1>,
) -> ClientMutationKey {
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), client_key.into());
    let mut record = RouterMutationRecord::new(1, created_at_ns, b"fp".to_vec());
    record.as_v1_mut().routing_in_progress = false;
    record.as_v1_mut().payload =
        crate::facade::stable::label_stats::RouterMutationPayloadV1::Scalar { shards };
    record.mark_terminal_at_ns(created_at_ns);
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
        m.insert(key.clone(), record);
    });
    key
}

fn bulk_load_gc_fixture_row(
    graph_id: GraphId,
    target: &BulkLoadTargetV1,
    parent_mutation_id: u64,
    chunk_index: u32,
    progress: BulkLoadChunkProgressV1,
) -> (BulkLoadChunkReceiptKey, BulkLoadChunkReceiptRecordV1) {
    let graph_request = BulkLoadGraphRequestV1::Vertex(OrderedVertexBatchGraphRequestV1 {
        graph_id,
        target_shard_id: target.shard_id,
        target_graph_canister: target.graph_canister,
        resolved_labels: ResolvedLabelTable::default(),
        resolved_properties: ResolvedPropertyTable::default(),
        items: vec![OrderedVertexBatchGraphItemV1 {
            resolved_vertex_labels: Vec::new(),
            resolved_initial_properties: Vec::new(),
        }],
    });
    let graph_request_fingerprint = graph_request.fingerprint().expect("Graph fingerprint");
    let chunk = BulkLoadChunkV1::Vertices(vec![AtomicInsertVertexV1 {
        vertex_labels: Vec::new(),
        initial_properties: Vec::new(),
    }]);
    let chunk_fingerprint = BulkLoadChunkEnvelopeV1::from_chunk(&chunk)
        .fingerprint()
        .expect("chunk fingerprint");
    let (graph_request, graph_request_fingerprint, public_receipt, graph_receipt, completed_at_ns) =
        match progress {
            BulkLoadChunkProgressV1::CanonicalPending => (
                Some(graph_request),
                Some(graph_request_fingerprint),
                None,
                None,
                None,
            ),
            BulkLoadChunkProgressV1::CanonicalCommitted
            | BulkLoadChunkProgressV1::ProjectionPending
            | BulkLoadChunkProgressV1::RetirementPending
            | BulkLoadChunkProgressV1::Completed => {
                let graph_receipt =
                    BulkLoadGraphReceiptV1::Vertex(GraphOrderedVertexBatchReceiptV1 {
                        logical_vertex_count: 1,
                        emitted_delta_first_seq: None,
                        emitted_delta_last_seq: None,
                        hot_forward_vertices: Vec::new(),
                        allocated_vertex_ids: vec![chunk_index + 1],
                    });
                let public_receipt = crate::types::AtomicInsertReceiptV1 {
                    logical_operation_count: 1,
                    logical_vertex_count: 1,
                    logical_edge_count: 0,
                    allocated_vertex_ids: vec![vec![
                        0;
                        gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
                    ]],
                };
                let completed_at_ns =
                    (progress == BulkLoadChunkProgressV1::Completed).then_some(10);
                // Completed rows are compacted; non-terminal rows retain the Graph request.
                let request =
                    (progress != BulkLoadChunkProgressV1::Completed).then_some(graph_request);
                let fingerprint = (progress != BulkLoadChunkProgressV1::Completed)
                    .then_some(graph_request_fingerprint);
                (
                    request,
                    fingerprint,
                    Some(public_receipt),
                    Some(graph_receipt),
                    completed_at_ns,
                )
            }
        };
    let row = BulkLoadChunkReceiptRecordV1 {
        chunk_fingerprint,
        graph_request,
        graph_request_fingerprint,
        child_mutation_id: parent_mutation_id + chunk_index as u64 + 1,
        progress,
        public_receipt,
        graph_receipt,
        completed_at_ns,
    };
    row.validate().expect("valid bulk-load fixture row");
    (
        BulkLoadChunkReceiptKey::new(parent_mutation_id, chunk_index),
        row,
    )
}

fn insert_bulk_load_gc_fixture(
    caller: Principal,
    client_key: &str,
    parent_mutation_id: u64,
    lifecycle: BulkLoadLifecycleV1,
    chunk_count: u32,
    terminal_at_ns: Option<u64>,
) -> ClientMutationKey {
    let graph_id = tenant_main_graph_id();
    let target = BulkLoadTargetV1 {
        shard_id: ShardId::new(0),
        graph_canister: graph_principal(200),
    };
    let mut coordinator = BulkLoadCoordinatorV1::new(target.clone());
    coordinator.logical_operation_count = chunk_count as u64;
    coordinator.logical_vertex_count = chunk_count as u64;
    coordinator.next_chunk_index = match &lifecycle {
        BulkLoadLifecycleV1::AppendPending { .. } => chunk_count.saturating_sub(1),
        BulkLoadLifecycleV1::Open
        | BulkLoadLifecycleV1::FinalizePending { .. }
        | BulkLoadLifecycleV1::Completed
        | BulkLoadLifecycleV1::Aborted
        | BulkLoadLifecycleV1::Failed { .. } => chunk_count,
        BulkLoadLifecycleV1::AbortPending { active_chunk } => *active_chunk,
    };
    coordinator.committed_chunk_count = match &lifecycle {
        BulkLoadLifecycleV1::AppendPending { .. } | BulkLoadLifecycleV1::AbortPending { .. } => {
            coordinator.next_chunk_index
        }
        _ => chunk_count,
    };
    coordinator.completed_chunk_count = match &lifecycle {
        BulkLoadLifecycleV1::AppendPending { .. } | BulkLoadLifecycleV1::AbortPending { .. } => {
            coordinator.next_chunk_index
        }
        _ => chunk_count,
    };
    coordinator.lifecycle = lifecycle.clone();
    coordinator
        .validate()
        .expect("valid bulk-load coordinator fixture");
    let mut record = RouterMutationRecord::new_bulk_load(parent_mutation_id, 0, coordinator)
        .expect("bulk-load parent");
    if let Some(terminal_at_ns) = terminal_at_ns {
        record.mark_terminal_at_ns(terminal_at_ns);
    }
    let key = ClientMutationKey::new(caller, graph_id, client_key.to_owned());
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key.clone(), record));
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| {
        for chunk_index in 0..chunk_count {
            let progress = if matches!(&lifecycle, BulkLoadLifecycleV1::AppendPending { .. })
                && chunk_index == chunk_count.saturating_sub(1)
            {
                BulkLoadChunkProgressV1::CanonicalPending
            } else {
                BulkLoadChunkProgressV1::Completed
            };
            let (row_key, row) = bulk_load_gc_fixture_row(
                graph_id,
                &target,
                parent_mutation_id,
                chunk_index,
                progress,
            );
            map.insert(row_key, row);
        }
    });
    key
}

#[test]
fn bulk_load_gc_owner_keeps_nonterminal_retryable_and_pending_child() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let caller = graph_principal(210);
    let pending_key = insert_bulk_load_gc_fixture(
        caller,
        "pending",
        9_700_001,
        BulkLoadLifecycleV1::AppendPending {
            chunk_index: 0,
            fingerprint: [1; 32],
            child_mutation_id: 9_700_002,
        },
        1,
        None,
    );
    let retryable_key = insert_mutation_record(caller, "retryable", 0, false);

    store
        .admin_sweep_expired_client_mutation_keys_at(
            admin,
            None,
            100,
            CLIENT_MUTATION_KEY_TTL_NS + 1,
        )
        .expect("sweep");

    assert!(ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| map.get(&retryable_key).is_some()));
    assert!(ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| map.get(&pending_key).is_some()));
    assert!(ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
        map.get(&BulkLoadChunkReceiptKey::new(9_700_001, 0))
            .is_some_and(|row| row.progress == BulkLoadChunkProgressV1::CanonicalPending)
    }));
}

#[test]
fn bulk_load_gc_owner_retains_terminal_parent_before_ttl() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
    let key = insert_bulk_load_gc_fixture(
        graph_principal(211),
        "fresh-terminal",
        9_700_101,
        BulkLoadLifecycleV1::Completed,
        1,
        Some(now - CLIENT_MUTATION_KEY_TTL_NS),
    );
    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100, now)
        .expect("sweep at retention boundary");

    assert!(ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| map.get(&key).is_some()));
    assert!(ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
        map.get(&BulkLoadChunkReceiptKey::new(9_700_101, 0))
            .is_some()
    }));
}

#[test]
fn bulk_load_gc_owner_resumes_bounded_cursor_and_removes_parent_last() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let now = CLIENT_MUTATION_KEY_TTL_NS + 1;
    let parent_id = 9_700_201;
    let chunk_count = crate::facade::stable::bulk_load::BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP + 1;
    let key = insert_bulk_load_gc_fixture(
        graph_principal(212),
        "large-terminal",
        parent_id,
        BulkLoadLifecycleV1::Completed,
        chunk_count,
        Some(0),
    );

    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100, now)
        .expect("first bounded sweep");
    let first = store
        .router_mutation_record(&key)
        .expect("parent retained while receipts remain");
    let RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) = first.payload() else {
        panic!("bulk parent payload");
    };
    assert_eq!(coordinator.lifecycle, BulkLoadLifecycleV1::Completed);
    assert_eq!(first.as_v1().terminal_at_ns, Some(0));
    assert_eq!(
        coordinator.receipt_gc_cursor,
        Some(crate::facade::stable::bulk_load::BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP)
    );
    assert_eq!(
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
            map.range((
                Bound::Included(BulkLoadChunkReceiptKey::new(parent_id, 0)),
                Bound::Unbounded,
            ))
            .take_while(|entry| entry.key().job_mutation_id == parent_id)
            .count()
        }),
        1
    );

    // A later sweep resumes from the durable parent cursor rather than restarting the scan.
    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100, now)
        .expect("resumed bounded sweep");
    store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100, now)
        .expect("parent removal sweep");
    assert!(ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| map.is_empty()));
    assert!(ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| map.get(&key).is_none()));
}

#[test]
fn bulk_load_finalize_step_stops_at_ssot_bound() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.clear_new());
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.clear_new());

    let caller = graph_principal(214);
    let parent_id = 9_700_401;
    let page = crate::facade::stable::bulk_load::BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP;
    let chunk_count = page * 3 + 1;
    let key = insert_bulk_load_gc_fixture(
        caller,
        "finalize-large",
        parent_id,
        BulkLoadLifecycleV1::FinalizePending {
            stage: crate::facade::stable::label_stats::BulkLoadFinalizeStageV1::VerifyReceipts,
            cursor: 0,
        },
        chunk_count,
        None,
    );

    for expected_cursor in [page, page * 2, page * 3] {
        BULK_LOAD_RECEIPT_ROW_READS.with(|reads| reads.set(0));
        let coordinator = store
            .finalize_bulk_load_step(caller, key.graph_id, &key.client_key, 10)
            .expect("bounded Finalize step");
        assert!(matches!(
            coordinator.lifecycle,
            BulkLoadLifecycleV1::FinalizePending { cursor, .. } if cursor == expected_cursor
        ));
        assert!(
            BULK_LOAD_RECEIPT_ROW_READS.with(|reads| reads.get()) <= page as usize,
            "Finalize read more than one SSOT page"
        );
    }

    BULK_LOAD_RECEIPT_ROW_READS.with(|reads| reads.set(0));
    let completed = store
        .finalize_bulk_load_step(caller, key.graph_id, &key.client_key, 10)
        .expect("final bounded Finalize step");
    assert_eq!(completed.lifecycle, BulkLoadLifecycleV1::Completed);
    assert!(
        BULK_LOAD_RECEIPT_ROW_READS.with(|reads| reads.get()) <= page as usize,
        "finalize completion rescanned the full job"
    );
}

#[test]
fn bulk_load_finalize_rejects_hole_and_extra_receipt_ranges() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(215);
    let page = crate::facade::stable::bulk_load::BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP;

    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.clear_new());
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.clear_new());
    let hole_parent = 9_700_501;
    let hole_key = insert_bulk_load_gc_fixture(
        caller,
        "finalize-hole",
        hole_parent,
        BulkLoadLifecycleV1::FinalizePending {
            stage: crate::facade::stable::label_stats::BulkLoadFinalizeStageV1::VerifyReceipts,
            cursor: 0,
        },
        2,
        None,
    );
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| {
        map.remove(&BulkLoadChunkReceiptKey::new(hole_parent, 1));
        let (row_key, row) = bulk_load_gc_fixture_row(
            hole_key.graph_id,
            &BulkLoadTargetV1 {
                shard_id: ShardId::new(0),
                graph_canister: graph_principal(200),
            },
            hole_parent,
            2,
            BulkLoadChunkProgressV1::Completed,
        );
        map.insert(row_key, row);
    });
    assert!(matches!(
        store.finalize_bulk_load_step(caller, hole_key.graph_id, &hole_key.client_key, 10),
        Err(RouterError::Conflict(message)) if message.contains("contiguous")
    ));

    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.clear_new());
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.clear_new());
    let extra_parent = 9_700_502;
    let extra_key = insert_bulk_load_gc_fixture(
        caller,
        "finalize-extra",
        extra_parent,
        BulkLoadLifecycleV1::FinalizePending {
            stage: crate::facade::stable::label_stats::BulkLoadFinalizeStageV1::VerifyReceipts,
            cursor: 0,
        },
        2,
        None,
    );
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| {
        let (row_key, row) = bulk_load_gc_fixture_row(
            extra_key.graph_id,
            &BulkLoadTargetV1 {
                shard_id: ShardId::new(0),
                graph_canister: graph_principal(200),
            },
            extra_parent,
            2,
            BulkLoadChunkProgressV1::Completed,
        );
        map.insert(row_key, row);
    });
    assert!(matches!(
        store.finalize_bulk_load_step(caller, extra_key.graph_id, &extra_key.client_key, 10),
        Err(RouterError::Conflict(message)) if message.contains("aggregate counters")
    ));
    assert_eq!(page, 32);
}

#[test]
fn bulk_load_receipt_gc_step_deletes_bound_and_persists_cursor() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.clear_new());
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.clear_new());

    let caller = graph_principal(216);
    let parent_id = 9_700_601;
    let page = crate::facade::stable::bulk_load::BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP;
    let chunk_count = page * 3 + 1;
    let key = insert_bulk_load_gc_fixture(
        caller,
        "gc-large",
        parent_id,
        BulkLoadLifecycleV1::Completed,
        chunk_count,
        Some(0),
    );
    let now = CLIENT_MUTATION_KEY_TTL_NS + 1;

    for expected_cursor in [page, page * 2, page * 3, page * 3 + 1] {
        BULK_LOAD_RECEIPT_ROW_READS.with(|reads| reads.set(0));
        let result = store
            .bulk_load_receipt_gc_step(caller, key.graph_id, &key.client_key, now)
            .expect("bounded receipt GC step");
        assert!(result.removed <= page);
        let record = store
            .router_mutation_record(&key)
            .expect("terminal parent retained while rows remain");
        let RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) = record.payload() else {
            panic!("bulk parent payload");
        };
        assert_eq!(coordinator.receipt_gc_cursor, Some(expected_cursor));
        assert!(
            BULK_LOAD_RECEIPT_ROW_READS.with(|reads| reads.get()) <= page as usize,
            "receipt GC rescanned beyond its bounded page"
        );
    }

    let done = store
        .bulk_load_receipt_gc_step(caller, key.graph_id, &key.client_key, now)
        .expect("parent removal step");
    assert!(done.done);
    assert!(store.router_mutation_record(&key).is_none());
}

#[test]
fn bulk_load_receipt_gc_rewinds_stale_cursor_before_parent_removal() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.clear_new());
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.clear_new());

    let caller = graph_principal(217);
    let parent_id = 9_700_701;
    let key = insert_bulk_load_gc_fixture(
        caller,
        "gc-stale-cursor",
        parent_id,
        BulkLoadLifecycleV1::Completed,
        1,
        Some(0),
    );
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| {
        let mut record = map.get(&key).expect("bulk parent");
        let RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) = record.payload_mut() else {
            panic!("bulk parent payload");
        };
        coordinator.receipt_gc_cursor = Some(99);
        map.insert(key.clone(), record);
    });

    let now = CLIENT_MUTATION_KEY_TTL_NS + 1;
    let rewound = store
        .bulk_load_receipt_gc_step(caller, key.graph_id, &key.client_key, now)
        .expect("stale cursor rewind");
    assert_eq!(rewound.removed, 0);
    assert!(!rewound.done);
    let record = store
        .router_mutation_record(&key)
        .expect("parent retained after rewind");
    let RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) = record.payload() else {
        panic!("bulk parent payload");
    };
    assert_eq!(coordinator.receipt_gc_cursor, Some(0));

    let removed = store
        .bulk_load_receipt_gc_step(caller, key.graph_id, &key.client_key, now)
        .expect("rewound row delete");
    assert_eq!(removed.removed, 1);
    let done = store
        .bulk_load_receipt_gc_step(caller, key.graph_id, &key.client_key, now)
        .expect("parent removal");
    assert!(done.done);
    assert!(store.router_mutation_record(&key).is_none());
}

#[test]
fn bulk_load_gc_owner_blocks_delayed_child_dispatch_and_commit() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let caller = graph_principal(213);
    let graph_id = tenant_main_graph_id();
    let parent_id = 9_700_301;
    let chunk_count = crate::facade::stable::bulk_load::BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP + 1;
    let key = insert_bulk_load_gc_fixture(
        caller,
        "delayed-child",
        parent_id,
        BulkLoadLifecycleV1::Completed,
        chunk_count,
        Some(0),
    );
    let target = BulkLoadTargetV1 {
        shard_id: ShardId::new(0),
        graph_canister: graph_principal(200),
    };
    let (_new_key, new_child) = bulk_load_gc_fixture_row(
        graph_id,
        &target,
        parent_id,
        chunk_count,
        BulkLoadChunkProgressV1::CanonicalPending,
    );
    store
        .admin_sweep_expired_client_mutation_keys_at(
            admin,
            None,
            100,
            CLIENT_MUTATION_KEY_TTL_NS + 1,
        )
        .expect("first GC step");
    let admit = store.admit_bulk_load_child(
        caller,
        graph_id,
        &key.client_key,
        parent_id,
        chunk_count,
        new_child.chunk_fingerprint,
        new_child,
    );
    assert!(matches!(admit, Err(RouterError::Conflict(_))));
    let existing = store
        .bulk_load_chunk_receipt(parent_id, chunk_count - 1)
        .expect("last retained receipt");
    let graph_receipt = existing.graph_receipt.clone().expect("Graph receipt");
    let public_receipt = existing.public_receipt.clone().expect("public receipt");
    let commit = store.record_bulk_load_canonical_committed(
        caller,
        graph_id,
        &key.client_key,
        parent_id,
        chunk_count - 1,
        existing.chunk_fingerprint,
        graph_receipt,
        public_receipt,
    );
    assert!(matches!(commit, Err(RouterError::Conflict(_))));

    store
        .admin_sweep_expired_client_mutation_keys_at(
            admin,
            None,
            100,
            CLIENT_MUTATION_KEY_TTL_NS + 1,
        )
        .expect("final GC step");
    store
        .admin_sweep_expired_client_mutation_keys_at(
            admin,
            None,
            100,
            CLIENT_MUTATION_KEY_TTL_NS + 1,
        )
        .expect("parent removal step");
    let (_late_key, late_child) = bulk_load_gc_fixture_row(
        graph_id,
        &target,
        parent_id,
        chunk_count,
        BulkLoadChunkProgressV1::CanonicalPending,
    );
    assert!(matches!(
        store.admit_bulk_load_child(
            caller,
            graph_id,
            &key.client_key,
            parent_id,
            chunk_count,
            late_child.chunk_fingerprint,
            late_child,
        ),
        Err(RouterError::NotFound(_))
    ));
    assert!(
        store
            .record_bulk_load_canonical_committed(
                caller,
                graph_id,
                &key.client_key,
                parent_id,
                chunk_count - 1,
                existing.chunk_fingerprint,
                existing.graph_receipt.unwrap(),
                existing.public_receipt.unwrap(),
            )
            .is_err()
    );
}

#[test]
fn ordered_batch_transition_persists_target_and_releases_routing_lease() {
    use crate::facade::stable::label_stats::{
        OrderedEdgeBatchTargetProgressV1, RouterMutationPayloadV1, RouterMutationRequestIdentityV1,
        RouterOrderedEdgeBatchTargetV1,
    };
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedEdgeBatchReceiptV1, OrderedEdgeBatchGraphItemV1, OrderedEdgeBatchGraphRequest,
        OrderedEdgeBatchGraphRequestV1, ordered_edge_batch_graph_request_fingerprint,
    };

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let caller = graph_principal(1);
    let client_key = "ordered-transition";
    let public_fingerprint = [7; 32];
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), client_key.to_owned());
    store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            client_key,
            public_fingerprint.to_vec(),
        )
        .expect("ordered reservation");

    let request = OrderedEdgeBatchGraphRequestV1 {
        graph_id: tenant_main_graph_id(),
        target_shard_id: ShardId::new(0),
        target_graph_canister: graph_principal(9),
        resolved_labels: Default::default(),
        resolved_properties: Default::default(),
        items: vec![OrderedEdgeBatchGraphItemV1 {
            source_local_vertex_id: 10,
            target_local_vertex_id: 11,
            directed: true,
            catalog_edge_label_id: None,
            inline_property_bytes: Vec::new(),
            resolved_initial_edge_properties: Vec::new(),
        }],
    };
    let graph_request = OrderedEdgeBatchGraphRequest::V1(request.clone());
    let graph_fingerprint = ordered_edge_batch_graph_request_fingerprint(&graph_request)
        .expect("Graph request fingerprint");
    let target = RouterOrderedEdgeBatchTargetV1 {
        graph_request_fingerprint: graph_fingerprint,
        request,
        progress: OrderedEdgeBatchTargetProgressV1::CanonicalPending,
        projection_watermark: None,
    };
    let reserved = store
        .router_mutation_record(&key)
        .expect("reserved edge record");
    let err = store
        .transition_to_ordered_edge_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedVertexBatch {
                public_fingerprint,
                public_item_count: 1,
            },
            Default::default(),
            Default::default(),
            target.clone(),
        )
        .expect_err("wrong edge identity family must be rejected");
    assert_eq!(
        err,
        RouterError::InvalidArgument(
            "ordered edge batch transition requires an OrderedEdgeBatch request identity".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after rejected admission"),
        reserved
    );
    let err = store
        .transition_to_ordered_edge_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedEdgeBatch {
                public_fingerprint,
                public_item_count: 2,
            },
            Default::default(),
            Default::default(),
            target.clone(),
        )
        .expect_err("mismatched edge item count must be rejected");
    assert_eq!(
        err,
        RouterError::InvalidArgument(
            "ordered public item count does not match Graph request".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after rejected count admission"),
        reserved
    );
    store
        .transition_to_ordered_edge_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedEdgeBatch {
                public_fingerprint,
                public_item_count: 1,
            },
            Default::default(),
            Default::default(),
            target,
        )
        .expect("ordered transition");

    let record = store.router_mutation_record(&key).expect("ordered record");
    assert!(!record.as_v1().routing_in_progress);
    assert!(matches!(
        record.as_v1().request_identity,
        RouterMutationRequestIdentityV1::OrderedEdgeBatch {
            public_item_count: 1,
            ..
        }
    ));
    assert!(matches!(
        record.payload(),
        RouterMutationPayloadV1::OrderedEdgeBatch(replay)
            if matches!(replay.target.progress, OrderedEdgeBatchTargetProgressV1::CanonicalPending)
    ));
    assert_eq!(record.as_v1().routing_lease_ns, None);

    let receipt = GraphOrderedEdgeBatchReceiptV1 {
        logical_edge_count: 1,
        emitted_delta_first_seq: None,
        emitted_delta_last_seq: None,
        hot_forward_vertices: Vec::new(),
    };
    let before_canonical = store
        .router_mutation_record(&key)
        .expect("edge canonical-pending record");
    let wrong_canonical_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_edge_batch_canonical_committed(
            &key,
            1,
            wrong_canonical_fingerprint,
            receipt.clone(),
        )
        .expect_err("a different Graph fingerprint must be rejected before canonical commit");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered canonical completion".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after canonical fingerprint rejection"),
        before_canonical,
        "a rejected canonical fingerprint must leave the durable record unchanged"
    );
    store
        .record_ordered_edge_batch_canonical_committed(&key, 1, graph_fingerprint, receipt.clone())
        .expect("canonical completion");
    let canonical_committed = store
        .router_mutation_record(&key)
        .expect("edge canonical-committed record");
    store
        .record_ordered_edge_batch_canonical_committed(&key, 1, graph_fingerprint, receipt.clone())
        .expect("idempotent canonical completion");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after canonical replay"),
        canonical_committed,
        "an exact canonical replay must leave the durable record unchanged"
    );
    let mut mismatched_receipt = receipt.clone();
    mismatched_receipt.hot_forward_vertices = vec![7];
    mismatched_receipt
        .validate()
        .expect("mismatched edge canonical receipt remains valid");
    let err = store
        .record_ordered_edge_batch_canonical_committed(
            &key,
            1,
            graph_fingerprint,
            mismatched_receipt,
        )
        .expect_err("a different canonical receipt must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered canonical completion conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after canonical receipt rejection"),
        canonical_committed,
        "a rejected canonical receipt must leave the durable record unchanged"
    );
    let before_projection_pending = store
        .router_mutation_record(&key)
        .expect("edge canonical record before projection pending");
    let wrong_pending_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_edge_batch_projection_pending(&key, 1, wrong_pending_fingerprint)
        .expect_err("a different Graph fingerprint must be rejected before projection pending");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered projection pending".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after projection-pending fingerprint rejection"),
        before_projection_pending,
        "a rejected projection-pending fingerprint must leave the durable record unchanged"
    );
    store
        .record_ordered_edge_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect("projection pending");
    let projection_pending = store
        .router_mutation_record(&key)
        .expect("edge projection-pending record");
    store
        .record_ordered_edge_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect("idempotent projection pending");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after projection-pending replay"),
        projection_pending,
        "an exact projection-pending replay must leave the durable record unchanged"
    );
    let err = store
        .record_ordered_edge_batch_canonical_committed(&key, 1, graph_fingerprint, receipt.clone())
        .expect_err("canonical commit after projection pending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered canonical completion conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after canonical progress conflict"),
        projection_pending,
        "a canonical progress conflict must leave the durable record unchanged"
    );
    let before_projection_advanced = projection_pending;
    let watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(0),
        label_stats_seq: None,
    };
    let wrong_shard_watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(1),
        label_stats_seq: None,
    };
    let err = store
        .record_ordered_edge_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            wrong_shard_watermark,
        )
        .expect_err("a watermark for another shard must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict("ordered projection watermark targets a different shard".into())
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after wrong-shard rejection"),
        before_projection_advanced,
        "a rejected projection watermark must leave the durable record unchanged"
    );
    let wrong_projection_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_edge_batch_projection_advanced(
            &key,
            1,
            wrong_projection_fingerprint,
            watermark.clone(),
        )
        .expect_err("a different Graph fingerprint must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered projection advancement".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after projection fingerprint rejection"),
        before_projection_advanced,
        "a rejected projection fingerprint must leave the durable record unchanged"
    );
    store
        .record_ordered_edge_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            watermark.clone(),
        )
        .expect("projection advanced");
    let projection_advanced = store
        .router_mutation_record(&key)
        .expect("edge projection-advanced record");
    store
        .record_ordered_edge_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            watermark.clone(),
        )
        .expect("idempotent projection advancement");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after projection-advanced replay"),
        projection_advanced,
        "an exact projection-advanced replay must leave the durable record unchanged"
    );
    let conflicting_watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(0),
        label_stats_seq: Some(1),
    };
    let err = store
        .record_ordered_edge_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            conflicting_watermark,
        )
        .expect_err("a different projection watermark must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered projection advancement conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after projection watermark conflict"),
        projection_advanced,
        "a conflicting projection watermark must leave the durable record unchanged"
    );
    let err = store
        .record_ordered_edge_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect_err("projection pending after advancement must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered projection pending conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after projection-pending progress conflict"),
        projection_advanced,
        "a projection-pending progress conflict must leave the durable record unchanged"
    );
    let before_retirement_pending = store
        .router_mutation_record(&key)
        .expect("projected edge record");
    let err = store
        .record_ordered_edge_batch_retired(&key, 1, graph_fingerprint, receipt.clone())
        .expect_err("retirement completion before RetirementPending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered retirement completion requires RetirementPending progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after premature retirement completion"),
        before_retirement_pending,
        "premature retirement completion must leave the durable record unchanged"
    );
    store
        .record_ordered_edge_batch_retirement_pending(&key, 1, graph_fingerprint)
        .expect("retirement pending");
    let retirement_pending = store
        .router_mutation_record(&key)
        .expect("edge retirement-pending record");
    store
        .record_ordered_edge_batch_retirement_pending(&key, 1, graph_fingerprint)
        .expect("idempotent retirement-pending replay");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after retirement-pending replay"),
        retirement_pending,
        "exact retirement-pending replay must leave the durable record unchanged"
    );
    let mut mismatched_pending_receipt = receipt.clone();
    mismatched_pending_receipt.hot_forward_vertices = vec![7];
    mismatched_pending_receipt
        .validate()
        .expect("mismatched pending edge receipt remains valid");
    let err = store
        .record_ordered_edge_batch_retired(&key, 1, graph_fingerprint, mismatched_pending_receipt)
        .expect_err("different receipt at RetirementPending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered retirement completion requires RetirementPending progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after pending receipt rejection"),
        retirement_pending,
        "rejected pending receipt must leave the durable record unchanged"
    );
    store
        .record_ordered_edge_batch_retired(&key, 1, graph_fingerprint, receipt.clone())
        .expect("retirement completion");
    let record = store
        .router_mutation_record(&key)
        .expect("completed ordered record");
    assert!(matches!(
        record.payload(),
        RouterMutationPayloadV1::CompletedOrderedEdgeBatch {
            graph_request_fingerprint: persisted_graph_fingerprint,
            receipt,
            projection_watermark,
        } if persisted_graph_fingerprint == &graph_fingerprint
            && receipt.logical_edge_count == 1
            && projection_watermark.shard_id == ShardId::new(0)
    ));
    assert_eq!(record.as_v1().completed_row_count, Some(1));
    let mismatched_graph_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_edge_batch_retired(&key, 1, mismatched_graph_fingerprint, receipt.clone())
        .expect_err("mismatched terminal Graph fingerprint must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered retirement completion".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("record after fingerprint rejection"),
        record,
        "mismatched terminal fingerprint must leave the durable record unchanged"
    );
    let mut mismatched_receipt = receipt.clone();
    mismatched_receipt.hot_forward_vertices = vec![7];
    mismatched_receipt
        .validate()
        .expect("mismatched edge receipt remains valid");
    let err = store
        .record_ordered_edge_batch_retired(&key, 1, graph_fingerprint, mismatched_receipt)
        .expect_err("mismatched terminal receipt must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered retirement completion requires an ordered replay payload".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("record after receipt rejection"),
        record,
        "mismatched terminal receipt must leave the durable record unchanged"
    );
    store
        .record_ordered_edge_batch_retired(&key, 1, graph_fingerprint, receipt)
        .expect("idempotent retirement completion");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("edge record after terminal retirement replay"),
        record,
        "exact terminal retirement replay must leave the durable record unchanged"
    );
}

#[test]
fn ordered_mixed_batch_transition_persists_phase_counts_and_replay_target() {
    use crate::facade::stable::label_stats::{
        OrderedMixedBatchTargetProgressV1, RouterMutationPayloadV1,
        RouterMutationRequestIdentityV1, RouterOrderedMixedBatchTargetV1,
    };
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedMixedBatchReceiptV1, OrderedMixedBatchGraphRequest,
        OrderedMixedBatchGraphRequestV1, OrderedMixedGraphEdgeItemV1, OrderedMixedGraphEndpointV1,
        OrderedMixedGraphOperationV1, OrderedVertexBatchGraphItemV1,
        ordered_mixed_batch_graph_request_fingerprint,
    };

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let caller = graph_principal(1);
    let client_key = "ordered-mixed-transition";
    let public_fingerprint = [8; 32];
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), client_key.to_owned());
    store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            client_key,
            public_fingerprint.to_vec(),
        )
        .expect("mixed reservation");

    let request = OrderedMixedBatchGraphRequestV1 {
        graph_id: tenant_main_graph_id(),
        target_shard_id: ShardId::new(0),
        target_graph_canister: graph_principal(9),
        resolved_labels: Default::default(),
        resolved_properties: Default::default(),
        operations: vec![
            OrderedMixedGraphOperationV1::Vertex(OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: Vec::new(),
                resolved_initial_properties: Vec::new(),
            }),
            OrderedMixedGraphOperationV1::Edge(OrderedMixedGraphEdgeItemV1 {
                source: OrderedMixedGraphEndpointV1::NewVertexOrdinal(0),
                target: OrderedMixedGraphEndpointV1::NewVertexOrdinal(0),
                directed: false,
                catalog_edge_label_id: None,
                inline_property_bytes: Vec::new(),
                resolved_initial_edge_properties: Vec::new(),
            }),
        ],
    };
    let graph_request = OrderedMixedBatchGraphRequest::V1(request.clone());
    let graph_fingerprint = ordered_mixed_batch_graph_request_fingerprint(&graph_request)
        .expect("mixed Graph request fingerprint");
    let target = RouterOrderedMixedBatchTargetV1 {
        graph_request_fingerprint: graph_fingerprint,
        request,
        progress: OrderedMixedBatchTargetProgressV1::CanonicalPending,
        projection_watermark: None,
    };
    let reserved = store
        .router_mutation_record(&key)
        .expect("reserved mixed record");
    let err = store
        .transition_to_ordered_mixed_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedEdgeBatch {
                public_fingerprint,
                public_item_count: 2,
            },
            Default::default(),
            Default::default(),
            target.clone(),
        )
        .expect_err("wrong mixed identity family must be rejected");
    assert_eq!(
        err,
        RouterError::InvalidArgument(
            "ordered mixed batch transition requires an OrderedMixedBatch request identity".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after rejected admission"),
        reserved
    );
    let err = store
        .transition_to_ordered_mixed_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedMixedBatch {
                public_fingerprint,
                public_operation_count: 2,
                public_vertex_count: 2,
                public_edge_count: 0,
            },
            Default::default(),
            Default::default(),
            target.clone(),
        )
        .expect_err("mismatched mixed phase counts must be rejected");
    assert_eq!(
        err,
        RouterError::InvalidArgument(
            "ordered mixed phase counts do not match Graph request".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after rejected count admission"),
        reserved
    );
    store
        .transition_to_ordered_mixed_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedMixedBatch {
                public_fingerprint,
                public_operation_count: 2,
                public_vertex_count: 1,
                public_edge_count: 1,
            },
            Default::default(),
            Default::default(),
            target,
        )
        .expect("mixed transition");

    let record = store.router_mutation_record(&key).expect("mixed record");
    assert!(!record.as_v1().routing_in_progress);
    assert!(matches!(
        record.as_v1().request_identity,
        RouterMutationRequestIdentityV1::OrderedMixedBatch {
            public_operation_count: 2,
            public_vertex_count: 1,
            public_edge_count: 1,
            ..
        }
    ));
    assert!(matches!(
        record.payload(),
        RouterMutationPayloadV1::OrderedMixedBatch(replay)
            if matches!(replay.target.progress, OrderedMixedBatchTargetProgressV1::CanonicalPending)
    ));

    let receipt = GraphOrderedMixedBatchReceiptV1 {
        logical_operation_count: 2,
        logical_vertex_count: 1,
        logical_edge_count: 1,
        emitted_delta_first_seq: None,
        emitted_delta_last_seq: None,
        hot_forward_vertices: Vec::new(),
        allocated_vertex_ids: vec![7],
    };
    let before_canonical = store
        .router_mutation_record(&key)
        .expect("mixed canonical-pending record");
    let wrong_canonical_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_mixed_batch_canonical_committed(
            &key,
            1,
            wrong_canonical_fingerprint,
            receipt.clone(),
        )
        .expect_err("a different mixed Graph fingerprint must be rejected before canonical commit");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered mixed canonical completion".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after canonical fingerprint rejection"),
        before_canonical,
        "a rejected mixed canonical fingerprint must leave the durable record unchanged"
    );
    store
        .record_ordered_mixed_batch_canonical_committed(&key, 1, graph_fingerprint, receipt.clone())
        .expect("mixed canonical receipt");
    let canonical_committed = store
        .router_mutation_record(&key)
        .expect("mixed canonical record");
    assert!(matches!(
        canonical_committed.payload(),
        RouterMutationPayloadV1::OrderedMixedBatch(replay)
            if matches!(replay.target.progress, OrderedMixedBatchTargetProgressV1::CanonicalCommitted(ref persisted) if persisted == &receipt)
    ));
    store
        .record_ordered_mixed_batch_canonical_committed(&key, 1, graph_fingerprint, receipt.clone())
        .expect("idempotent mixed canonical receipt");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after canonical replay"),
        canonical_committed,
        "an exact mixed canonical replay must leave the durable record unchanged"
    );
    let mut mismatched_receipt = receipt.clone();
    mismatched_receipt.allocated_vertex_ids = vec![8];
    mismatched_receipt
        .validate()
        .expect("mismatched mixed canonical receipt remains valid");
    let err = store
        .record_ordered_mixed_batch_canonical_committed(
            &key,
            1,
            graph_fingerprint,
            mismatched_receipt,
        )
        .expect_err("a different mixed canonical receipt must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed canonical completion conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after canonical receipt rejection"),
        canonical_committed,
        "a rejected mixed canonical receipt must leave the durable record unchanged"
    );
    let before_projection_pending = canonical_committed;
    let wrong_pending_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_mixed_batch_projection_pending(&key, 1, wrong_pending_fingerprint)
        .expect_err(
            "a different mixed Graph fingerprint must be rejected before projection pending",
        );
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered mixed projection pending".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after projection-pending fingerprint rejection"),
        before_projection_pending,
        "a rejected mixed projection-pending fingerprint must leave the durable record unchanged"
    );
    store
        .record_ordered_mixed_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect("mixed projection pending");
    let projection_pending = store
        .router_mutation_record(&key)
        .expect("mixed projection-pending record");
    store
        .record_ordered_mixed_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect("idempotent mixed projection pending");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after projection-pending replay"),
        projection_pending,
        "an exact mixed projection-pending replay must leave the durable record unchanged"
    );
    let err = store
        .record_ordered_mixed_batch_canonical_committed(&key, 1, graph_fingerprint, receipt.clone())
        .expect_err("mixed canonical commit after projection pending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed canonical completion conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after canonical progress conflict"),
        projection_pending,
        "a mixed canonical progress conflict must leave the durable record unchanged"
    );
    let before_projection_advanced = projection_pending;
    let watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(0),
        label_stats_seq: None,
    };
    let wrong_shard_watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(1),
        label_stats_seq: None,
    };
    let err = store
        .record_ordered_mixed_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            wrong_shard_watermark,
        )
        .expect_err("a mixed watermark for another shard must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed projection watermark targets a different shard".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after wrong-shard rejection"),
        before_projection_advanced,
        "a rejected mixed projection watermark must leave the durable record unchanged"
    );
    let wrong_projection_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_mixed_batch_projection_advanced(
            &key,
            1,
            wrong_projection_fingerprint,
            watermark.clone(),
        )
        .expect_err("a different mixed Graph fingerprint must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered mixed projection advancement".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after projection fingerprint rejection"),
        before_projection_advanced,
        "a rejected mixed projection fingerprint must leave the durable record unchanged"
    );
    store
        .record_ordered_mixed_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            watermark.clone(),
        )
        .expect("mixed projection advanced");
    let projection_advanced = store
        .router_mutation_record(&key)
        .expect("mixed projected record");
    store
        .record_ordered_mixed_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            watermark.clone(),
        )
        .expect("idempotent mixed projection advancement");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after projection-advanced replay"),
        projection_advanced,
        "an exact mixed projection-advanced replay must leave the durable record unchanged"
    );
    let conflicting_watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(0),
        label_stats_seq: Some(1),
    };
    let err = store
        .record_ordered_mixed_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            conflicting_watermark,
        )
        .expect_err("a different mixed projection watermark must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed projection advancement conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after projection watermark conflict"),
        projection_advanced,
        "a conflicting mixed projection watermark must leave the durable record unchanged"
    );
    let err = store
        .record_ordered_mixed_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect_err("mixed projection pending after advancement must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed projection pending conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after projection-pending progress conflict"),
        projection_advanced,
        "a mixed projection-pending progress conflict must leave the durable record unchanged"
    );
    let record = projection_advanced;
    assert!(matches!(
        record.payload(),
        RouterMutationPayloadV1::OrderedMixedBatch(replay)
            if matches!(replay.target.progress, OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(_))
                && replay.target.projection_watermark.is_some()
    ));
    let before_retirement_pending = record;
    let err = store
        .record_ordered_mixed_batch_retired(&key, 1, graph_fingerprint, receipt.clone())
        .expect_err("mixed retirement completion before RetirementPending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed retirement completion requires RetirementPending progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after premature retirement completion"),
        before_retirement_pending,
        "premature mixed retirement completion must leave the durable record unchanged"
    );
    store
        .record_ordered_mixed_batch_retirement_pending(&key, 1, graph_fingerprint)
        .expect("mixed retirement pending");
    let retirement_pending = store
        .router_mutation_record(&key)
        .expect("mixed retirement-pending record");
    store
        .record_ordered_mixed_batch_retirement_pending(&key, 1, graph_fingerprint)
        .expect("idempotent mixed retirement-pending replay");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after retirement-pending replay"),
        retirement_pending,
        "exact mixed retirement-pending replay must leave the durable record unchanged"
    );
    let mut mismatched_pending_receipt = receipt.clone();
    mismatched_pending_receipt.allocated_vertex_ids = vec![8];
    mismatched_pending_receipt
        .validate()
        .expect("mismatched pending mixed receipt remains valid");
    let err = store
        .record_ordered_mixed_batch_retired(&key, 1, graph_fingerprint, mismatched_pending_receipt)
        .expect_err("different mixed receipt at RetirementPending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed retirement completion requires RetirementPending progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after pending receipt rejection"),
        retirement_pending,
        "rejected pending mixed receipt must leave the durable record unchanged"
    );
    store
        .record_ordered_mixed_batch_retired(&key, 1, graph_fingerprint, receipt.clone())
        .expect("mixed retirement complete");
    let record = store
        .router_mutation_record(&key)
        .expect("mixed completed record");
    assert!(matches!(
        record.payload(),
        RouterMutationPayloadV1::CompletedOrderedMixedBatch {
            graph_request_fingerprint: persisted_graph_fingerprint,
            receipt: persisted,
            projection_watermark: _,
        } if persisted_graph_fingerprint == &graph_fingerprint && persisted == &receipt
    ));
    assert_eq!(record.as_v1().completed_row_count, Some(2));
    let mismatched_graph_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_mixed_batch_retired(&key, 1, mismatched_graph_fingerprint, receipt.clone())
        .expect_err("mismatched terminal mixed Graph fingerprint must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered mixed retirement completion".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after fingerprint rejection"),
        record,
        "mismatched terminal mixed fingerprint must leave the durable record unchanged"
    );
    let mut mismatched_receipt = receipt.clone();
    mismatched_receipt.allocated_vertex_ids = vec![8];
    mismatched_receipt
        .validate()
        .expect("mismatched mixed receipt remains valid");
    let err = store
        .record_ordered_mixed_batch_retired(&key, 1, graph_fingerprint, mismatched_receipt)
        .expect_err("mismatched terminal mixed receipt must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered mixed retirement completion requires an ordered replay payload".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after receipt rejection"),
        record,
        "mismatched terminal mixed receipt must leave the durable record unchanged"
    );
    store
        .record_ordered_mixed_batch_retired(&key, 1, graph_fingerprint, receipt)
        .expect("idempotent mixed retirement completion");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("mixed record after terminal retirement replay"),
        record,
        "exact terminal mixed retirement replay must leave the durable record unchanged"
    );
}

#[test]
fn ordered_vertex_batch_lifecycle_rejects_out_of_order_advance_without_mutation() {
    use crate::facade::stable::label_stats::{
        OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1,
        RouterMutationRequestIdentityV1, RouterOrderedVertexBatchTargetV1,
    };
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedVertexBatchReceiptV1, OrderedVertexBatchGraphRequest,
        ordered_vertex_batch_graph_request_fingerprint,
    };

    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    let caller = graph_principal(1);
    let client_key = "ordered-vertex-lifecycle";
    let key = ClientMutationKey::new(caller, tenant_main_graph_id(), client_key.to_owned());
    let public_fingerprint = [9; 32];
    store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            client_key,
            public_fingerprint.to_vec(),
        )
        .expect("vertex reservation");

    let request = OrderedVertexBatchGraphRequestV1 {
        graph_id: tenant_main_graph_id(),
        target_shard_id: ShardId::new(0),
        target_graph_canister: graph_principal(9),
        resolved_labels: Default::default(),
        resolved_properties: Default::default(),
        items: vec![OrderedVertexBatchGraphItemV1 {
            resolved_vertex_labels: Vec::new(),
            resolved_initial_properties: Vec::new(),
        }],
    };
    let graph_request = OrderedVertexBatchGraphRequest::V1(request.clone());
    let graph_fingerprint = ordered_vertex_batch_graph_request_fingerprint(&graph_request)
        .expect("vertex Graph request fingerprint");
    let target = RouterOrderedVertexBatchTargetV1 {
        graph_request_fingerprint: graph_fingerprint,
        request,
        progress: OrderedVertexBatchTargetProgressV1::CanonicalPending,
        projection_watermark: None,
    };
    let reserved = store
        .router_mutation_record(&key)
        .expect("reserved vertex record");
    let err = store
        .transition_to_ordered_vertex_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedEdgeBatch {
                public_fingerprint,
                public_item_count: 1,
            },
            Default::default(),
            Default::default(),
            target.clone(),
        )
        .expect_err("wrong vertex identity family must be rejected");
    assert_eq!(
        err,
        RouterError::InvalidArgument(
            "ordered vertex batch transition requires an OrderedVertexBatch request identity"
                .into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after rejected admission"),
        reserved
    );
    let err = store
        .transition_to_ordered_vertex_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedVertexBatch {
                public_fingerprint,
                public_item_count: 2,
            },
            Default::default(),
            Default::default(),
            target.clone(),
        )
        .expect_err("mismatched vertex item count must be rejected");
    assert_eq!(
        err,
        RouterError::InvalidArgument(
            "ordered public vertex item count does not match Graph request".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after rejected count admission"),
        reserved
    );
    store
        .transition_to_ordered_vertex_batch(
            &key,
            1,
            RouterMutationRequestIdentityV1::OrderedVertexBatch {
                public_fingerprint,
                public_item_count: 1,
            },
            Default::default(),
            Default::default(),
            target,
        )
        .expect("vertex transition");

    let record = store.router_mutation_record(&key).expect("vertex record");
    assert_eq!(
        record.as_v1().request_identity,
        RouterMutationRequestIdentityV1::OrderedVertexBatch {
            public_fingerprint,
            public_item_count: 1,
        }
    );

    let receipt = GraphOrderedVertexBatchReceiptV1 {
        logical_vertex_count: 1,
        emitted_delta_first_seq: None,
        emitted_delta_last_seq: None,
        hot_forward_vertices: Vec::new(),
        allocated_vertex_ids: vec![7],
    };
    store
        .record_ordered_vertex_batch_canonical_committed(
            &key,
            1,
            graph_fingerprint,
            receipt.clone(),
        )
        .expect("vertex canonical completion");

    let before = store
        .router_mutation_record(&key)
        .expect("canonical record");
    let wrong_pending_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_vertex_batch_projection_pending(&key, 1, wrong_pending_fingerprint)
        .expect_err(
            "a different vertex Graph fingerprint must be rejected before projection pending",
        );
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered vertex projection pending".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after projection-pending fingerprint rejection"),
        before,
        "a rejected vertex projection-pending fingerprint must leave the durable record unchanged"
    );
    let err = store
        .record_ordered_vertex_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            gleaph_graph_kernel::plan_exec::MutationTokenShard {
                shard_id: ShardId::new(0),
                label_stats_seq: None,
            },
        )
        .expect_err("out-of-order projection advancement must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered vertex projection advancement conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("record after rejection"),
        before,
        "rejected phase transition must leave the durable record unchanged"
    );

    store
        .record_ordered_vertex_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect("vertex projection pending");
    let projection_pending = store
        .router_mutation_record(&key)
        .expect("vertex projection-pending record");
    store
        .record_ordered_vertex_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect("idempotent vertex projection pending");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after projection-pending replay"),
        projection_pending,
        "an exact vertex projection-pending replay must leave the durable record unchanged"
    );
    let before_projection_advanced = projection_pending;
    let watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(0),
        label_stats_seq: None,
    };
    let wrong_shard_watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(1),
        label_stats_seq: None,
    };
    let err = store
        .record_ordered_vertex_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            wrong_shard_watermark,
        )
        .expect_err("a vertex watermark for another shard must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered vertex projection watermark targets a different shard".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after wrong-shard rejection"),
        before_projection_advanced,
        "a rejected vertex projection watermark must leave the durable record unchanged"
    );
    let wrong_projection_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_vertex_batch_projection_advanced(
            &key,
            1,
            wrong_projection_fingerprint,
            watermark.clone(),
        )
        .expect_err("a different vertex Graph fingerprint must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered vertex projection advancement".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after projection fingerprint rejection"),
        before_projection_advanced,
        "a rejected vertex projection fingerprint must leave the durable record unchanged"
    );
    store
        .record_ordered_vertex_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            watermark.clone(),
        )
        .expect("vertex projection advanced");
    let projection_advanced = store
        .router_mutation_record(&key)
        .expect("vertex projection-advanced record");
    store
        .record_ordered_vertex_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            watermark.clone(),
        )
        .expect("idempotent vertex projection advancement");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after projection-advanced replay"),
        projection_advanced,
        "an exact vertex projection-advanced replay must leave the durable record unchanged"
    );
    let conflicting_watermark = gleaph_graph_kernel::plan_exec::MutationTokenShard {
        shard_id: ShardId::new(0),
        label_stats_seq: Some(1),
    };
    let err = store
        .record_ordered_vertex_batch_projection_advanced(
            &key,
            1,
            graph_fingerprint,
            conflicting_watermark,
        )
        .expect_err("a different vertex projection watermark must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered vertex projection advancement conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after projection watermark conflict"),
        projection_advanced,
        "a conflicting vertex projection watermark must leave the durable record unchanged"
    );
    let err = store
        .record_ordered_vertex_batch_projection_pending(&key, 1, graph_fingerprint)
        .expect_err("vertex projection pending after advancement must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered vertex projection pending conflicts with persisted progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after projection-pending progress conflict"),
        projection_advanced,
        "a vertex projection-pending progress conflict must leave the durable record unchanged"
    );
    let before_retirement_pending = store
        .router_mutation_record(&key)
        .expect("projected vertex record");
    let err = store
        .record_ordered_vertex_batch_retired(&key, 1, graph_fingerprint, receipt.clone())
        .expect_err("vertex retirement completion before RetirementPending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered vertex retirement completion requires RetirementPending progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after premature retirement completion"),
        before_retirement_pending,
        "premature vertex retirement completion must leave the durable record unchanged"
    );
    store
        .record_ordered_vertex_batch_retirement_pending(&key, 1, graph_fingerprint)
        .expect("vertex retirement pending");
    let retirement_pending = store
        .router_mutation_record(&key)
        .expect("vertex retirement-pending record");
    store
        .record_ordered_vertex_batch_retirement_pending(&key, 1, graph_fingerprint)
        .expect("idempotent vertex retirement-pending replay");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after retirement-pending replay"),
        retirement_pending,
        "exact vertex retirement-pending replay must leave the durable record unchanged"
    );
    let mut mismatched_pending_receipt = receipt.clone();
    mismatched_pending_receipt.allocated_vertex_ids = vec![8];
    mismatched_pending_receipt
        .validate()
        .expect("mismatched pending vertex receipt remains valid");
    let err = store
        .record_ordered_vertex_batch_retired(&key, 1, graph_fingerprint, mismatched_pending_receipt)
        .expect_err("different vertex receipt at RetirementPending must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered vertex retirement completion requires RetirementPending progress".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after pending receipt rejection"),
        retirement_pending,
        "rejected pending vertex receipt must leave the durable record unchanged"
    );
    store
        .record_ordered_vertex_batch_retired(&key, 1, graph_fingerprint, receipt.clone())
        .expect("vertex retirement complete");

    let record = store
        .router_mutation_record(&key)
        .expect("completed vertex record");
    assert!(matches!(
        record.payload(),
        RouterMutationPayloadV1::CompletedOrderedVertexBatch {
            graph_request_fingerprint: persisted_graph_fingerprint,
            receipt: persisted,
            projection_watermark,
        } if persisted_graph_fingerprint == &graph_fingerprint
            && persisted == &receipt
            && projection_watermark == &watermark
    ));
    assert_eq!(record.as_v1().completed_row_count, Some(1));
    let mismatched_graph_fingerprint = {
        let mut fingerprint = graph_fingerprint;
        fingerprint[0] ^= 1;
        fingerprint
    };
    let err = store
        .record_ordered_vertex_batch_retired(&key, 1, mismatched_graph_fingerprint, receipt.clone())
        .expect_err("mismatched terminal vertex Graph fingerprint must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "Graph request fingerprint mismatch at ordered vertex retirement completion".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after fingerprint rejection"),
        record,
        "mismatched terminal vertex fingerprint must leave the durable record unchanged"
    );
    let mut mismatched_receipt = receipt.clone();
    mismatched_receipt.allocated_vertex_ids = vec![8];
    mismatched_receipt
        .validate()
        .expect("mismatched vertex receipt remains valid");
    let err = store
        .record_ordered_vertex_batch_retired(&key, 1, graph_fingerprint, mismatched_receipt)
        .expect_err("mismatched terminal vertex receipt must be rejected");
    assert_eq!(
        err,
        RouterError::Conflict(
            "ordered vertex retirement completion requires an ordered replay payload".into()
        )
    );
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after receipt rejection"),
        record,
        "mismatched terminal vertex receipt must leave the durable record unchanged"
    );
    store
        .record_ordered_vertex_batch_retired(&key, 1, graph_fingerprint, receipt)
        .expect("idempotent vertex retirement completion");
    assert_eq!(
        store
            .router_mutation_record(&key)
            .expect("vertex record after terminal retirement replay"),
        record,
        "exact terminal vertex retirement replay must leave the durable record unchanged"
    );
}

// ADR 0029 Phase 4: TTL eviction must retain non-terminal sagas (recovery targets) and only
// ADR 0029 Phase 4: TTL eviction must retain non-terminal sagas (recovery targets) and only
// reclaim terminal ones; the old "not routing" rule wrongly stranded committed-but-unprojected
// federated mutations.
#[test]
fn ttl_eviction_retains_retryable_failure_until_later_terminal_completion_expires() {
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
    // Retryable failure: routing released without an envelope and no canonical write. Its age does
    // not start terminal retention, so it survives no matter how old `created_at_ns` is.
    let failed = insert_mutation_record(graph_principal(2), "failed", 0, false);

    let result = store
        .admin_sweep_expired_client_mutation_keys_at(admin, None, 100, now)
        .expect("sweep");
    assert_eq!(result.removed, 0);
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
        assert!(
            m.get(&pending).is_some(),
            "non-terminal saga must be retained as a recovery target"
        );
        let retryable = m.get(&failed).expect("retryable failure survives");
        assert!(!retryable.is_terminal());
        assert_eq!(retryable.as_v1().terminal_at_ns, None);
    });

    // A successful same-key retry can later complete. That transition starts, rather than
    // backdates, the seven-day retention window.
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
        let mut record = m.get(&failed).expect("retryable record");
        record.as_v1_mut().completed_row_count = Some(0);
        record.mark_terminal_at_ns(now);
        m.insert(failed.clone(), record);
    });
    store
        .admin_sweep_expired_client_mutation_keys_at(
            admin,
            None,
            100,
            now + CLIENT_MUTATION_KEY_TTL_NS,
        )
        .expect("sweep at retention boundary");
    assert!(ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&failed).is_some()));
    store
        .admin_sweep_expired_client_mutation_keys_at(
            admin,
            None,
            100,
            now + CLIENT_MUTATION_KEY_TTL_NS + 1,
        )
        .expect("sweep after retention boundary");
    assert!(ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&failed).is_none()));
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
    record.as_v1_mut().routing_in_progress = false;
    record.as_v1_mut().terminal_failure = Some("permanent".into());
    record.mark_terminal_at_ns(0);
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.insert(key.clone(), record));
    assert!(
        store
            .router_mutation_record(&key)
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
    record.as_v1_mut().routing_in_progress = false;
    record.as_v1_mut().terminal_failure = Some("permanent".into());
    record.mark_terminal_at_ns(0);
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

// ADR 0029 Phase 4: recovery diagnostics attach to every non-terminal state, including retryable
// failure, remain bounded for public status sizing, and never resurrect an irreversible terminal
// record.
#[test]
fn record_last_error_bounds_live_diagnostics_and_skips_terminal() {
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
    assert_eq!(live.as_v1().last_error.as_deref(), Some("boom"));

    let failed = insert_mutation_record(graph_principal(2), "failed", 0, false);
    store
        .record_router_mutation_last_error(
            &failed,
            format!(
                "a{}",
                "é".repeat(
                    crate::facade::stable::label_stats::MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES
                )
            ),
        )
        .expect("record bounded retryable diagnostic");
    let retryable = ROUTER_MUTATION_BY_CLIENT_KEY
        .with_borrow(|m| m.get(&failed))
        .expect("record");
    assert!(!retryable.is_terminal());
    assert_eq!(
        retryable.as_v1().last_error.as_ref().unwrap().len(),
        crate::facade::stable::label_stats::MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES - 1,
        "the byte bound must not split the final two-byte character"
    );
    assert!(
        retryable
            .as_v1()
            .last_error
            .as_ref()
            .unwrap()
            .ends_with('é')
    );

    let terminal = insert_mutation_record_with_shards(
        graph_principal(3),
        "terminal",
        0,
        vec![shard_with(0, false, false)],
    );
    assert!(store.terminally_fail_uncommitted_dispatch(&terminal, 1, "permanent".into()));
    store
        .record_router_mutation_last_error(&terminal, "ignored".into())
        .expect("no-op on terminal");
    let terminal = ROUTER_MUTATION_BY_CLIENT_KEY
        .with_borrow(|m| m.get(&terminal))
        .expect("terminal record");
    assert!(
        terminal.as_v1().last_error.is_none(),
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
    let expired = insert_completed_mutation_record(graph_principal(1), "expired", 0);
    let fresh = insert_completed_mutation_record(graph_principal(2), "fresh", now);
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
        insert_completed_mutation_record(graph_principal(100 + i), "expired", 0);
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
    let key = test_mutation_key(caller, tenant_main_graph_id(), "client-key-1");

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
            &key,
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            vec![RouterMutationShardV1::new(
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
    let key = test_mutation_key(caller, tenant_main_graph_id(), "client-key-1");

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
        .abandon_router_mutation_routing_reservation(&key)
        .expect("abandon reservation");
    let record = store.router_mutation_record(&key).expect("record");
    assert_eq!(record.as_v1().mutation_id, first.mutation_id);
    assert_eq!(record.as_v1().request_identity.request_fingerprint(), b"a");
    assert!(!record.as_v1().routing_in_progress);

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
    let key = test_mutation_key(caller, tenant_main_graph_id(), "client-key-1");
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
            &key,
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            vec![
                RouterMutationShardV1::new(ShardId::new(0), graph_principal(1), Some(vec![1])),
                RouterMutationShardV1::new(ShardId::new(1), graph_principal(2), None),
            ],
        )
        .expect("record shards");
    let record = store.router_mutation_record(&key).expect("record");
    assert_eq!(
        record.as_v1().resolved_labels,
        Some(ResolvedLabelTable::default())
    );
    assert_eq!(store.router_mutation_completed_row_count(&key), None);

    store
        .record_router_mutation_shard_completed(&key, ShardId::new(0), 2)
        .expect("complete shard 0");
    store
        .record_router_mutation_shard_projection_advanced(&key, ShardId::new(0))
        .expect("advance projection shard 0");
    assert_eq!(store.router_mutation_completed_row_count(&key), None);

    store
        .record_router_mutation_shard_completed(&key, ShardId::new(1), 3)
        .expect("complete shard 1");
    store
        .record_router_mutation_shard_projection_advanced(&key, ShardId::new(1))
        .expect("advance projection shard 1");
    assert_eq!(store.router_mutation_completed_row_count(&key), Some(5));

    // ADR 0025 (E): once fully completed + projected, the heavy fields are dropped and
    // the final row count is pinned, so replay still returns Some(5) from a small record.
    let compacted = store.router_mutation_record(&key).expect("record");
    assert_eq!(compacted.as_v1().completed_row_count, Some(5));
    assert!(
        compacted.shards().is_empty(),
        "shard fan-out must be dropped"
    );
    assert!(
        compacted.as_v1().resolved_labels.is_none()
            && compacted.as_v1().resolved_properties.is_none(),
        "resolved tables must be dropped after completion"
    );
}

#[test]
fn record_router_mutation_shards_is_idempotent_after_partial_completion() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let caller = graph_principal(42);
    let key = test_mutation_key(caller, tenant_main_graph_id(), "client-key-1");

    store
        .reserve_mutation_id_for_client_key(
            caller,
            tenant_main_graph_id(),
            "client-key-1",
            b"a".to_vec(),
        )
        .expect("mutation id");
    let shards = vec![
        RouterMutationShardV1::new(ShardId::new(0), graph_principal(1), Some(vec![1])),
        RouterMutationShardV1::new(ShardId::new(1), graph_principal(2), None),
    ];
    store
        .record_router_mutation_shards(
            &key,
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            shards.clone(),
        )
        .expect("record initial envelope");

    // Simulate the first shard completing before the saga retried.
    store
        .record_router_mutation_shard_completed(&key, ShardId::new(0), 2)
        .expect("complete shard 0");

    // A retry rebuilds the same envelope from the durable record. The writer must
    // accept it as idempotent, not conflict because the stored shard is completed.
    store
        .record_router_mutation_shards(
            &key,
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            shards,
        )
        .expect("retry with the same envelope is idempotent");

    let record = store.router_mutation_record(&key).expect("record");
    let stored_shards = record.shards();
    assert_eq!(stored_shards.len(), 2);
    assert!(stored_shards[0].completed());
    assert_eq!(stored_shards[0].row_count(), 2);
    assert!(!stored_shards[1].completed());
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
        insert_completed_mutation_record(graph_principal(50 + i), "old", 0);
    }
    let fresh = insert_completed_mutation_record(graph_principal(200), "fresh", now);
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
    let key = test_mutation_key(caller, tenant_main_graph_id(), "client-key-1");
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
            &key,
            ResolvedLabelTable::default(),
            ResolvedPropertyTable::default(),
            0,
        )
        .expect("record zero-shard completion");

    let record = store.router_mutation_record(&key).expect("record");
    assert_eq!(record.as_v1().completed_row_count, Some(0));
    assert!(record.shards().is_empty());
    assert_eq!(store.router_mutation_completed_row_count(&key), Some(0));
    store
        .record_router_mutation_shard_completed(&key, ShardId::new(0), 0)
        .expect("terminal replay completion is idempotent");
    store
        .record_router_mutation_shard_projection_advanced(&key, ShardId::new(0))
        .expect("terminal replay projection is idempotent");
}

pub(crate) mod graph_type_catalog_vocabulary {
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
        apply_catalog_statement_block(&catalog_block_from(&ddl), &ddl).expect("apply ddl");

        assert!(store.lookup_vertex_label_id(graph_id, "Person").is_ok());
        assert!(store.lookup_edge_label_id(graph_id, "KNOWS").is_ok());
        assert!(store.lookup_property_id(graph_id, "name").is_ok());
    }

    #[test]
    fn graph_type_order_by_insertion_resolves_label_policy() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        let ddl = "CREATE GRAPH g { NODE Person, DIRECTED EDGE FeedMembership LABEL IN_FEED ORDER BY INSERTION CONNECTING (Person -> Person) }";
        apply_catalog_statement_block(&catalog_block_from(ddl), ddl).expect("apply ddl");

        assert_eq!(
            store
                .lookup_edge_ordering_policy(graph_id, "IN_FEED")
                .expect("policy"),
            gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy::Insertion
        );
        // Labels not declared in the Graph Type resolve to the Unordered default.
        assert_eq!(
            store
                .lookup_edge_ordering_policy(graph_id, "UNDECLARED")
                .expect("policy"),
            gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy::Unordered
        );
    }

    #[test]
    fn graph_type_order_by_insertion_applies_to_all_declared_labels() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        let ddl = "CREATE GRAPH g { NODE Person, DIRECTED EDGE Feed LABEL IN_FEED & IN_TOP_FEED ORDER BY INSERTION CONNECTING (Person -> Person) }";
        apply_catalog_statement_block(&catalog_block_from(ddl), ddl).expect("apply ddl");

        for label in ["IN_FEED", "IN_TOP_FEED"] {
            assert_eq!(
                store
                    .lookup_edge_ordering_policy(graph_id, label)
                    .expect("policy"),
                gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy::Insertion
            );
        }
    }

    #[test]
    fn graph_type_unknown_order_by_key_rejected_at_ddl() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");

        let ddl = "CREATE GRAPH g { NODE Person, DIRECTED EDGE FeedMembership LABEL IN_FEED ORDER BY SOMETHING CONNECTING (Person -> Person) }";
        let err = apply_catalog_statement_block(&catalog_block_from(ddl), ddl)
            .expect_err("unsupported ORDER BY key must fail at DDL");
        let msg = err.to_string();
        assert!(msg.contains("ORDER BY"), "expected ORDER BY mention: {msg}");
        assert!(msg.contains("SOMETHING"), "expected key mention: {msg}");
    }

    #[test]
    fn graph_type_conflicting_order_by_declarations_rejected_at_ddl() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");

        let ddl = "CREATE GRAPH g { NODE Person, DIRECTED EDGE A LABEL L ORDER BY INSERTION CONNECTING (Person -> Person), DIRECTED EDGE B LABEL L CONNECTING (Person -> Person) }";
        let err = apply_catalog_statement_block(&catalog_block_from(ddl), ddl)
            .expect_err("conflicting per-label policies must fail at DDL");
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting"),
            "expected conflict mention: {msg}"
        );
    }

    fn seed_edge_live_count(store: &RouterStore, graph_id: GraphId, label: &str, count: i64) {
        let label_id = store
            .lookup_edge_label_id(graph_id, label)
            .expect("label id");
        store.apply_label_stats_delta_payload(
            graph_id,
            ShardId::new(0),
            &LabelStatsDelta {
                vertex: vec![],
                edge: vec![(label_id, count)],
            },
        );
    }

    const INLINE_GRAPH_UNORDERED: &str = "CREATE GRAPH g { NODE Person, DIRECTED EDGE FeedMembership LABEL IN_FEED CONNECTING (Person -> Person) }";
    const INLINE_GRAPH_INSERTION: &str = "CREATE GRAPH g { NODE Person, DIRECTED EDGE FeedMembership LABEL IN_FEED ORDER BY INSERTION CONNECTING (Person -> Person) }";
    const OR_REPLACE_GRAPH_INSERTION: &str = "CREATE OR REPLACE GRAPH g { NODE Person, DIRECTED EDGE FeedMembership LABEL IN_FEED ORDER BY INSERTION CONNECTING (Person -> Person) }";

    #[test]
    fn inline_policy_change_with_live_edges_rejected_at_ddl() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        apply_catalog_statement_block(
            &catalog_block_from(INLINE_GRAPH_UNORDERED),
            INLINE_GRAPH_UNORDERED,
        )
        .expect("apply initial graph");
        seed_edge_live_count(&store, graph_id, "IN_FEED", 1);

        let err = apply_catalog_statement_block(
            &catalog_block_from(OR_REPLACE_GRAPH_INSERTION),
            OR_REPLACE_GRAPH_INSERTION,
        )
        .expect_err("policy change on a live label must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("IN_FEED"), "expected label mention: {msg}");
        assert!(msg.contains("live"), "expected live-count mention: {msg}");
        // The catalog binding must be untouched (still the Unordered definition).
        assert_eq!(
            store
                .lookup_edge_ordering_policy(graph_id, "IN_FEED")
                .expect("policy unchanged"),
            gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy::Unordered
        );
    }

    #[test]
    fn inline_policy_change_with_zero_live_edges_accepted_at_ddl() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        apply_catalog_statement_block(
            &catalog_block_from(INLINE_GRAPH_UNORDERED),
            INLINE_GRAPH_UNORDERED,
        )
        .expect("apply initial graph");
        seed_edge_live_count(&store, graph_id, "IN_FEED", 0);

        apply_catalog_statement_block(
            &catalog_block_from(OR_REPLACE_GRAPH_INSERTION),
            OR_REPLACE_GRAPH_INSERTION,
        )
        .expect("zero-live-edge policy change must be accepted");
        assert_eq!(
            store
                .lookup_edge_ordering_policy(graph_id, "IN_FEED")
                .expect("policy"),
            gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy::Insertion
        );
    }

    #[test]
    fn inline_unchanged_policy_with_live_edges_accepted_at_ddl() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        apply_catalog_statement_block(
            &catalog_block_from(INLINE_GRAPH_INSERTION),
            INLINE_GRAPH_INSERTION,
        )
        .expect("apply initial graph");
        seed_edge_live_count(&store, graph_id, "IN_FEED", 5);

        apply_catalog_statement_block(
            &catalog_block_from(OR_REPLACE_GRAPH_INSERTION),
            OR_REPLACE_GRAPH_INSERTION,
        )
        .expect("unchanged policy must be accepted even with live edges");
    }

    #[test]
    fn named_type_policy_change_with_live_edges_rejected_at_ddl() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        let ddl = "CREATE GRAPH TYPE gt { NODE Person, DIRECTED EDGE FeedMembership LABEL IN_FEED CONNECTING (Person -> Person) } NEXT CREATE GRAPH g TYPED gt";
        apply_catalog_statement_block(&catalog_block_from(ddl), ddl).expect("apply typed graph");
        seed_edge_live_count(&store, graph_id, "IN_FEED", 1);

        let ddl2 = "CREATE OR REPLACE GRAPH TYPE gt { NODE Person, DIRECTED EDGE FeedMembership LABEL IN_FEED ORDER BY INSERTION CONNECTING (Person -> Person) }";
        let err = apply_catalog_statement_block(&catalog_block_from(ddl2), ddl2)
            .expect_err("named type policy change on a live label must fail closed");
        assert!(err.to_string().contains("IN_FEED"));
        // The type replacement must not have been applied: graph still Unordered.
        assert_eq!(
            store
                .lookup_edge_ordering_policy(graph_id, "IN_FEED")
                .expect("policy unchanged"),
            gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy::Unordered
        );
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
        apply_catalog_statement_block(&catalog_block_from(&ddl), &ddl).expect("apply ddl");

        assert!(store.lookup_vertex_label_id(graph_id, "Person").is_ok());
        assert!(store.lookup_edge_label_id(graph_id, "KNOWS").is_ok());
        assert!(store.lookup_property_id(graph_id, "name").is_ok());
    }

    #[test]
    fn create_graph_type_edge_inline_ddl_registers_schema() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        let ddl = "CREATE GRAPH TYPE gt { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { distance FLOAT32 INLINE } CONNECTING (city -> city) } NEXT CREATE GRAPH g TYPED gt";
        apply_catalog_statement_block(&catalog_block_from(ddl), ddl).expect("apply ddl");

        let label_id = store.lookup_edge_label_id(graph_id, "ROAD").expect("label");
        let property_id = store
            .lookup_property_id(graph_id, "distance")
            .expect("property");
        assert_eq!(
            ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow(|s| s.get_record(graph_id, label_id)),
            Some(EdgeInlinePropertySchemaRecord::InlineScalar {
                property_id,
                scalar_type: InlineScalarType::F32,
            })
        );
    }

    #[test]
    fn create_graph_type_nested_record_inline_ddl_flattens_schema_paths() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");
        let ddl = "CREATE GRAPH TYPE gt { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { stats RECORD { score FLOAT32, meta RECORD { source UINT16 } } INLINE } CONNECTING (city -> city) } NEXT CREATE GRAPH g TYPED gt";
        apply_catalog_statement_block(&catalog_block_from(ddl), ddl).expect("apply ddl");

        let label_id = store.lookup_edge_label_id(graph_id, "ROAD").expect("label");
        let (_, schema) = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow(|s| s.get_profile_and_inline_schema(graph_id, label_id));
        let Some(ResolvedInlineSchema::Struct { fields, .. }) = schema else {
            panic!("expected struct schema");
        };
        assert_eq!(
            fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            vec!["score", "meta.source"]
        );
    }

    #[test]
    fn create_graph_any_skips_vocabulary_intern() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "g");
        let graph_id = lookup_graph_id("g").expect("graph id");

        apply_catalog_statement_block(
            &catalog_block_from("CREATE GRAPH g ANY"),
            "CREATE GRAPH g ANY",
        )
        .expect("apply ddl");

        assert!(matches!(
            store.lookup_vertex_label_id(graph_id, "Person"),
            Err(RouterError::NotFound(_))
        ));
    }

    // --- ADR 0031 Slice 4: vector dispatch activation + per-graph readiness ---

    pub(crate) fn setup_one_shard_graph(store: &RouterStore, admin: Principal) -> GraphId {
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
    /// has a resolved graph target to match shard attachments against (ADR 0031 Slice 4). The
    /// embedding name `vec{index_id}` is interned so the public `vector_search` can resolve by name.
    pub(crate) fn register_vector_def(graph_id: GraphId, index_id: u32, target: Principal) {
        let name = format!("vec{index_id}");
        let name_id =
            crate::facade::stable::embedding_name_catalog::intern_embedding_name(graph_id, &name)
                .expect("intern embedding name");
        let index_name_id =
            crate::facade::stable::index_name_catalog::intern_index_name(graph_id, &name)
                .expect("intern index name");
        crate::facade::stable::vector_index_catalog::register_vector_index(
            graph_id,
            index_id,
            index_name_id,
            name_id,
            vec![gleaph_graph_kernel::entry::VertexLabelId::from_raw(1)],
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
                vector_canister: graph_principal(7),
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
                    vector_canister: target,
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
                vector_canister: graph_principal(9),
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
                vector_canister: Principal::anonymous(),
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

// -----------------------------------------------------------------------------
// ADR 0034 Slice 20: inline edge scalar schema tests
// -----------------------------------------------------------------------------

use crate::facade::stable::edge_inline_property_profiles::{
    EdgeInlinePropertySchemaRecord, InlineScalarType,
};

#[test]
fn inline_scalar_schema_creates_label_property_and_record() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();

    store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "ROAD",
            "distance",
            InlineScalarType::F32,
        )
        .expect("create inline scalar schema");

    let label_id = store.lookup_edge_label_id(graph_id, "ROAD").expect("label");
    let property_id = store
        .lookup_property_id(graph_id, "distance")
        .expect("property");
    let record = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
        .with_borrow(|s| s.get_record(graph_id, label_id))
        .expect("schema record");
    assert_eq!(
        record,
        EdgeInlinePropertySchemaRecord::InlineScalar {
            property_id,
            scalar_type: InlineScalarType::F32,
        }
    );
    assert_eq!(
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow(|s| s.get_profile(graph_id, label_id)),
        InlineScalarType::F32.edge_inline_property_profile()
    );
}

#[test]
fn inline_scalar_schema_is_idempotent() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();

    store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "ROAD",
            "distance",
            InlineScalarType::F32,
        )
        .expect("first");
    store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "ROAD",
            "distance",
            InlineScalarType::F32,
        )
        .expect("idempotent");
}

#[test]
fn inline_scalar_schema_conflicts_with_legacy_profile() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    store
        .admin_intern_edge_label(admin, "tenant.main", "ROAD")
        .expect("edge label");
    store
        .admin_set_edge_label_inline_property_profile(
            admin,
            "tenant.main",
            "ROAD",
            EdgeInlinePropertyProfile {
                byte_width: 2,
                encoding: EdgeInlinePropertyEncoding::RawU16,
            },
        )
        .expect("legacy profile");

    let err = store
        .commit_set_edge_label_inline_scalar_schema(
            tenant_main_graph_id(),
            "ROAD",
            "distance",
            InlineScalarType::U16,
        )
        .expect_err("conflict");
    assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
}

#[test]
fn legacy_profile_setter_conflicts_with_inline_scalar() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");

    store
        .commit_set_edge_label_inline_scalar_schema(
            tenant_main_graph_id(),
            "ROAD",
            "distance",
            InlineScalarType::F32,
        )
        .expect("inline scalar");

    let err = store
        .admin_set_edge_label_inline_property_profile(
            admin,
            "tenant.main",
            "ROAD",
            EdgeInlinePropertyProfile {
                byte_width: 2,
                encoding: EdgeInlinePropertyEncoding::RawU16,
            },
        )
        .expect_err("conflict");
    assert!(matches!(err, RouterError::InvalidArgument(_)), "{err:?}");
}

#[test]
fn inline_scalar_schema_conflicts_on_different_scalar() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();

    store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "ROAD",
            "distance",
            InlineScalarType::F32,
        )
        .expect("first");
    let err = store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "ROAD",
            "distance",
            InlineScalarType::F64,
        )
        .expect_err("conflict");
    assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
}

#[test]
fn inline_scalar_schema_conflicts_on_second_property() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.main");
    let graph_id = tenant_main_graph_id();

    store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "ROAD",
            "distance",
            InlineScalarType::F32,
        )
        .expect("first");
    RouterStore::commit_intern_property_name(graph_id, "time").expect("property");
    let err = store
        .commit_set_edge_label_inline_scalar_schema(graph_id, "ROAD", "time", InlineScalarType::U32)
        .expect_err("conflict");
    assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
}

// -----------------------------------------------------------------------------
// ADR 0034 Slice 24: inline edge STRUCT schema tests
// -----------------------------------------------------------------------------

use crate::facade::stable::edge_inline_property_profiles::InlineStructLayout;

#[test]
fn inline_struct_schema_creates_label_property_and_record() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.struct");
    let graph_id = lookup_graph_id("tenant.struct").expect("graph");

    store
        .commit_set_edge_label_inline_struct_schema(
            graph_id,
            "AFFINITY",
            "stats",
            vec![
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "score".into(),
                    scalar_type: InlineScalarType::F32,
                },
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "confidence".into(),
                    scalar_type: InlineScalarType::F32,
                },
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "updated_at".into(),
                    scalar_type: InlineScalarType::U64,
                },
            ],
        )
        .expect("create inline struct schema");

    let label_id = store
        .lookup_edge_label_id(graph_id, "AFFINITY")
        .expect("label");
    let property_id = store
        .lookup_property_id(graph_id, "stats")
        .expect("property");
    let record = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
        .with_borrow(|s| s.get_record(graph_id, label_id))
        .expect("schema record");
    let layout = InlineStructLayout::from_fields(vec![
        ("score".into(), InlineScalarType::F32),
        ("confidence".into(), InlineScalarType::F32),
        ("updated_at".into(), InlineScalarType::U64),
    ])
    .expect("layout");
    assert_eq!(
        record,
        EdgeInlinePropertySchemaRecord::InlineStruct {
            property_id,
            field_specs: layout.field_specs().to_vec(),
        }
    );
    assert_eq!(
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow(|s| s.get_profile(graph_id, label_id)),
        EdgeInlinePropertyProfile::opaque_bytes(16)
    );

    // Slice 25: the resolved wire projection derived from the canonical record must contain the
    // exact physical field layout; Graph validates it but never persists it.
    let (wire_profile, wire_schema) = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
        .with_borrow(|s| s.get_profile_and_inline_schema(graph_id, label_id));
    assert_eq!(wire_profile, EdgeInlinePropertyProfile::opaque_bytes(16));
    let schema = wire_schema.expect("struct schema projected");
    assert_eq!(schema.property_id(), property_id);
    let ResolvedInlineSchema::Struct { fields, .. } = schema else {
        panic!("expected struct schema");
    };
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].name, "score");
    assert_eq!(fields[0].byte_offset, 0);
    assert_eq!(
        fields[0].profile,
        InlineScalarType::F32.edge_inline_property_profile()
    );
    assert_eq!(fields[1].name, "confidence");
    assert_eq!(fields[1].byte_offset, 4);
    assert_eq!(
        fields[1].profile,
        InlineScalarType::F32.edge_inline_property_profile()
    );
    assert_eq!(fields[2].name, "updated_at");
    assert_eq!(fields[2].byte_offset, 8);
    assert_eq!(
        fields[2].profile,
        InlineScalarType::U64.edge_inline_property_profile()
    );
}

#[test]
fn inline_struct_schema_is_idempotent() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.struct_idem");
    let graph_id = lookup_graph_id("tenant.struct_idem").expect("graph");

    let fields = vec![
        crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
            name: "score".into(),
            scalar_type: InlineScalarType::F32,
        },
        crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
            name: "confidence".into(),
            scalar_type: InlineScalarType::F32,
        },
    ];

    store
        .commit_set_edge_label_inline_struct_schema(graph_id, "AFFINITY", "stats", fields.clone())
        .expect("first");
    store
        .commit_set_edge_label_inline_struct_schema(graph_id, "AFFINITY", "stats", fields)
        .expect("idempotent");
}

#[test]
fn inline_struct_schema_conflicts_with_scalar() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.struct_scalar_conflict");
    let graph_id = lookup_graph_id("tenant.struct_scalar_conflict").expect("graph");

    store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "AFFINITY",
            "stats",
            InlineScalarType::F32,
        )
        .expect("scalar");

    let err = store
        .commit_set_edge_label_inline_struct_schema(
            graph_id,
            "AFFINITY",
            "stats",
            vec![
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "score".into(),
                    scalar_type: InlineScalarType::F32,
                },
            ],
        )
        .expect_err("conflict");
    assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
}

#[test]
fn inline_scalar_schema_conflicts_with_struct() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.scalar_struct_conflict");
    let graph_id = lookup_graph_id("tenant.scalar_struct_conflict").expect("graph");

    store
        .commit_set_edge_label_inline_struct_schema(
            graph_id,
            "AFFINITY",
            "stats",
            vec![
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "score".into(),
                    scalar_type: InlineScalarType::F32,
                },
            ],
        )
        .expect("struct");

    let err = store
        .commit_set_edge_label_inline_scalar_schema(
            graph_id,
            "AFFINITY",
            "stats",
            InlineScalarType::F32,
        )
        .expect_err("conflict");
    assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
}

#[test]
fn inline_struct_schema_conflicts_on_reordered_fields() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.struct_reorder");
    let graph_id = lookup_graph_id("tenant.struct_reorder").expect("graph");

    store
        .commit_set_edge_label_inline_struct_schema(
            graph_id,
            "AFFINITY",
            "stats",
            vec![
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "a".into(),
                    scalar_type: InlineScalarType::U8,
                },
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "b".into(),
                    scalar_type: InlineScalarType::U16,
                },
            ],
        )
        .expect("first");

    let err = store
        .commit_set_edge_label_inline_struct_schema(
            graph_id,
            "AFFINITY",
            "stats",
            vec![
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "b".into(),
                    scalar_type: InlineScalarType::U16,
                },
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: "a".into(),
                    scalar_type: InlineScalarType::U8,
                },
            ],
        )
        .expect_err("conflict");
    assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
}

#[test]
fn inline_struct_schema_failure_leaves_no_catalog_mutation() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.struct_no_partial");
    let graph_id = lookup_graph_id("tenant.struct_no_partial").expect("graph");

    let err = store
        .commit_set_edge_label_inline_struct_schema(graph_id, "AFFINITY", "stats", vec![])
        .expect_err("empty struct rejected");
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected InvalidArgument, got {err:?}"
    );

    // Neither the edge label nor the property name was interned by the failed commit.
    assert!(store.lookup_edge_label_id(graph_id, "AFFINITY").is_err());
    assert!(store.lookup_property_id(graph_id, "stats").is_err());
}

#[test]
fn inline_struct_schema_rejects_invalid_field_name() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.struct_bad_field");
    let graph_id = lookup_graph_id("tenant.struct_bad_field").expect("graph");

    let long_name = "a".repeat(300);
    let err = store
        .commit_set_edge_label_inline_struct_schema(
            graph_id,
            "AFFINITY",
            "stats",
            vec![
                crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                    name: long_name,
                    scalar_type: InlineScalarType::U8,
                },
            ],
        )
        .expect_err("invalid field name");
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected InvalidArgument, got {err:?}"
    );
    assert!(store.lookup_edge_label_id(graph_id, "AFFINITY").is_err());
    assert!(store.lookup_property_id(graph_id, "stats").is_err());
}

#[test]
fn inline_struct_schema_record_too_large_leaves_no_catalog_mutation() {
    let store = RouterStore::new();
    store.init_from_args(&test_init_args());
    let admin = Principal::from_slice(&[1; 29]);
    crate::facade::auth::grant_admins(&[admin]);
    register_test_graph(&store, admin, "tenant.struct_record_too_large");
    let graph_id = lookup_graph_id("tenant.struct_record_too_large").expect("graph");

    // Build a struct whose encoded stable record exceeds the 1024-byte envelope.
    let mut fields = Vec::new();
    for i in 0..64 {
        fields.push(
            crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                name: format!("very_long_field_name_to_inflate_record_size_{:03}", i),
                scalar_type:
                    crate::facade::stable::edge_inline_property_profiles::InlineScalarType::U8,
            },
        );
    }
    let err = store
        .commit_set_edge_label_inline_struct_schema(graph_id, "AFFINITY", "stats", fields)
        .expect_err("record-too-large struct rejected");
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "expected InvalidArgument, got {err:?}"
    );

    // Catalog preflight must reject before interning the label or property.
    assert!(store.lookup_edge_label_id(graph_id, "AFFINITY").is_err());
    assert!(store.lookup_property_id(graph_id, "stats").is_err());
}

// === ADR 0035 provisioning-request catalog tests =================================

#[cfg(test)]
mod provisioning_tests {
    use super::super::provisioning::{
        AckCommitError, ClearError, InsertError, InsertionOutcome, RouterProvisioningRequestStore,
    };
    use crate::facade::stable::ROUTER_PROVISIONING_INTENT_LOCK;
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;
    use crate::types::{
        CreatedResource, IntentLockOwner, ProvisionRequest, ProvisionResult,
        ProvisionResultOutcome, ProvisionableResource, ProvisionableResourceKind,
        ProvisioningIntentKey, ProvisioningRequestKey, RouterProvisionAck,
        RouterProvisioningRequest, RouterProvisioningRequestState,
    };
    use candid::{Decode, Encode, Principal};
    use gleaph_graph_kernel::entry::GraphId;
    use ic_stable_structures::Storable;

    fn store() -> RouterProvisioningRequestStore {
        // Reset the broader router store state so provisioning maps are clear.
        let router = RouterStore::new();
        router.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        RouterProvisioningRequestStore::new()
    }

    fn sample_request(request_id: &str, fingerprint: &str) -> RouterProvisioningRequest {
        RouterProvisioningRequest {
            request_id: request_id.to_owned(),
            request_fingerprint: fingerprint.to_owned(),
            caller: Principal::from_slice(&[1; 29]),
            graph_name: "tenant.main".to_owned(),
            reserved_graph_id: Some(GraphId::from_raw(7)),
            requested_resources: vec![ProvisionableResource {
                kind: ProvisionableResourceKind::GraphShard,
                logical_resource_key: "shard-0".to_owned(),
            }],
            state: RouterProvisioningRequestState::Pending,
            provision_receipt: None,
            accepted_registry_version: None,
            created_at_ns: 42,
        }
    }

    fn owner(request_id: &str, deployment_id: &str, fingerprint: &str) -> IntentLockOwner {
        IntentLockOwner::new(
            ProvisioningRequestKey::new(request_id, deployment_id),
            fingerprint.to_owned(),
        )
    }

    #[test]
    fn insert_returns_inserted_outcome_for_fresh_record() {
        let s = store();
        let req = sample_request("req-1", "fp-1");
        let outcome = s.insert("deploy-a", req.clone()).expect("insert");
        assert!(matches!(outcome, InsertionOutcome::Inserted(_)));
        let key = ProvisioningRequestKey::new("req-1", "deploy-a");
        let got = s.get_by_request_id(&key).expect("exists");
        assert_eq!(got, req);
    }

    #[test]
    fn insert_returns_existing_outcome_for_matching_fingerprint() {
        let s = store();
        let req = sample_request("req-1", "fp-1");
        let first = s.insert("deploy-a", req.clone()).expect("first");
        let InsertionOutcome::Inserted(first_record) = first else {
            panic!("first insert must be Inserted");
        };
        let mut second_req = req.clone();
        second_req.caller = Principal::from_slice(&[2; 29]);
        second_req.graph_name = "mutated.graph".to_owned();
        second_req.reserved_graph_id = Some(GraphId::from_raw(99));
        second_req.requested_resources = vec![ProvisionableResource {
            kind: ProvisionableResourceKind::PropertyIndex,
            logical_resource_key: "mutated".to_owned(),
        }];
        let second = s.insert("deploy-a", second_req).expect("second");
        assert_eq!(second, InsertionOutcome::Existing(first_record.clone()));
        assert_eq!(first_record.request_id, "req-1");
        assert_eq!(first_record.request_fingerprint, "fp-1");
        // Canonical store and derived index must still contain the first record.
        let key = ProvisioningRequestKey::new("req-1", "deploy-a");
        assert_eq!(s.get_by_request_id(&key), Some(first_record.clone()));
        let listed = s.list_by_graph("deploy-a", "tenant.main");
        assert_eq!(listed, vec![first_record]);
        // The mutated graph name from the second request was never stored.
        assert!(s.list_by_graph("deploy-a", "mutated.graph").is_empty());
    }

    #[test]
    fn insert_rejects_different_fingerprint_with_conflict() {
        let s = store();
        let mut req1 = sample_request("req-1", "fp-1");
        req1.graph_name = "graph-conflict".to_owned();
        let first = s.insert("deploy-a", req1.clone()).expect("insert");
        assert!(matches!(first, InsertionOutcome::Inserted(_)));
        let mut req2 = sample_request("req-1", "fp-2");
        req2.graph_name = "graph-conflict".to_owned();
        let err = s.insert("deploy-a", req2.clone()).expect_err("conflict");
        assert_eq!(err, InsertError::Conflict);
        // The first canonical record must survive unchanged.
        let key = ProvisioningRequestKey::new("req-1", "deploy-a");
        assert_eq!(s.get_by_request_id(&key), Some(req1.clone()));
        // The graph index entry must still resolve to the first request.
        let listed = s.list_by_graph("deploy-a", "graph-conflict");
        assert_eq!(listed, vec![req1.clone()]);
        // The first request's intent lock must remain held.
        let intent_key = ProvisioningIntentKey::new(
            "deploy-a",
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        assert!(s.intent_locked(&intent_key, &owner("req-1", "deploy-a", "fp-1")));
    }

    #[test]
    fn list_by_graph_returns_only_matching_graph() {
        let s = store();
        let mut req_a = sample_request("req-a", "fp-a");
        req_a.graph_name = "graph-a".to_owned();
        req_a.requested_resources[0].logical_resource_key = "res-a".to_owned();
        let mut req_b = sample_request("req-b", "fp-b");
        req_b.graph_name = "graph-b".to_owned();
        req_b.requested_resources[0].logical_resource_key = "res-b".to_owned();
        s.insert("deploy-a", req_a.clone()).expect("insert a");
        s.insert("deploy-a", req_b.clone()).expect("insert b");
        let listed = s.list_by_graph("deploy-a", "graph-a");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], req_a);
    }

    #[test]
    fn intent_lock_acquire() {
        let s = store();
        let req = sample_request("req-1", "fp-1");
        let key = ProvisioningIntentKey::new(
            "deploy-a",
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        assert!(!s.intent_locked(&key, &owner("req-1", "deploy-a", "fp-1")));
        s.insert("deploy-a", req).expect("insert");
        assert!(s.intent_locked(&key, &owner("req-1", "deploy-a", "fp-1")));
    }

    #[test]
    fn intent_lock_release() {
        let s = store();
        let req = sample_request("req-1", "fp-1");
        let key = ProvisioningIntentKey::new(
            "deploy-a",
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        s.insert("deploy-a", req).expect("insert");
        assert!(s.intent_locked(&key, &owner("req-1", "deploy-a", "fp-1")));
        s.clear_request(&ProvisioningRequestKey::new("req-1", "deploy-a"))
            .expect("clear");
        assert!(!s.intent_locked(&key, &owner("req-1", "deploy-a", "fp-1")));
    }

    #[test]
    fn cross_request_intent_exclusion_does_not_mutate_b() {
        let s = store();
        let mut req_a = sample_request("req-a", "fp-a");
        req_a.graph_name = "graph-x".to_owned();
        s.insert("deploy-a", req_a.clone()).expect("insert a");
        let intent_key = ProvisioningIntentKey::new(
            "deploy-a",
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        let mut req_b = sample_request("req-b", "fp-b");
        req_b.graph_name = "graph-x".to_owned();
        req_b.requested_resources = vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-0".to_owned(),
        }];
        let err = s
            .insert("deploy-a", req_b.clone())
            .expect_err("intent conflict");
        assert_eq!(err, InsertError::IntentConflict);
        // B's canonical record and graph-index entry must not exist.
        assert!(
            s.get_by_request_id(&ProvisioningRequestKey::new("req-b", "deploy-a"))
                .is_none()
        );
        let listed = s.list_by_graph("deploy-a", "graph-x");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].request_id, "req-a");
        // A's intent lock and canonical record must remain untouched.
        assert!(s.intent_locked(&intent_key, &owner("req-a", "deploy-a", "fp-a")));
        assert_eq!(
            s.get_by_request_id(&ProvisioningRequestKey::new("req-a", "deploy-a"))
                .map(|r| r.request_id),
            Some("req-a".to_owned())
        );
    }

    #[test]
    fn multi_resource_lock_lifecycle() {
        let s = store();
        let mut req = sample_request("req-m", "fp-m");
        req.requested_resources = vec![
            ProvisionableResource {
                kind: ProvisionableResourceKind::GraphShard,
                logical_resource_key: "shard-0".to_owned(),
            },
            ProvisionableResource {
                kind: ProvisionableResourceKind::PropertyIndex,
                logical_resource_key: "idx-0".to_owned(),
            },
        ];
        s.insert("deploy-a", req.clone()).expect("insert");
        let key1 = ProvisioningIntentKey::new(
            "deploy-a",
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        let key2 = ProvisioningIntentKey::new(
            "deploy-a",
            ProvisionableResourceKind::PropertyIndex,
            "idx-0",
        );
        assert!(s.intent_locked(&key1, &owner("req-m", "deploy-a", "fp-m")));
        assert!(s.intent_locked(&key2, &owner("req-m", "deploy-a", "fp-m")));
        s.clear_request(&ProvisioningRequestKey::new("req-m", "deploy-a"))
            .expect("clear");
        assert!(!s.intent_locked(&key1, &owner("req-m", "deploy-a", "fp-m")));
        assert!(!s.intent_locked(&key2, &owner("req-m", "deploy-a", "fp-m")));
    }

    #[test]
    fn duplicate_intent_in_one_request_rejects_without_mutation() {
        let s = store();
        let mut req = sample_request("req-d", "fp-d");
        req.requested_resources = vec![
            ProvisionableResource {
                kind: ProvisionableResourceKind::GraphShard,
                logical_resource_key: "same".to_owned(),
            },
            ProvisionableResource {
                kind: ProvisionableResourceKind::GraphShard,
                logical_resource_key: "same".to_owned(),
            },
        ];
        let err = s
            .insert("deploy-a", req.clone())
            .expect_err("duplicate intent");
        assert_eq!(err, InsertError::InvalidDuplicateIntent);
        assert!(
            s.get_by_request_id(&ProvisioningRequestKey::new("req-d", "deploy-a"))
                .is_none()
        );
        assert!(s.list_by_graph("deploy-a", "tenant.main").is_empty());
        let key =
            ProvisioningIntentKey::new("deploy-a", ProvisionableResourceKind::GraphShard, "same");
        assert!(!s.intent_locked(&key, &owner("req-d", "deploy-a", "fp-d")));
    }

    #[test]
    fn provision_request_candid_roundtrip() {
        let req = ProvisionRequest {
            deployment_id: "deploy-a".to_owned(),
            request_id: "req-1".to_owned(),
            request_fingerprint: "fp-1".to_owned(),
            intent_key: ProvisioningIntentKey::new(
                "deploy-a",
                ProvisionableResourceKind::GraphShard,
                "shard-0",
            ),
            reserved_graph_id: Some(GraphId::from_raw(7)),
            graph_name: "tenant.main".to_owned(),
            requested_resources: vec![ProvisionableResource {
                kind: ProvisionableResourceKind::GraphShard,
                logical_resource_key: "shard-0".to_owned(),
            }],
            authorized_caller: Principal::from_slice(&[2; 29]),
            release_id: "rel-1".to_owned(),
            router_callback_principal: Principal::from_slice(&[3; 29]),
        };
        let bytes = candid::Encode!(&req).expect("encode");
        let decoded = candid::Decode!(bytes.as_slice(), ProvisionRequest).expect("decode");
        assert_eq!(decoded, req);
    }

    #[test]
    fn provision_result_candid_roundtrip() {
        let result = ProvisionResult {
            request_id: "req-1".to_owned(),
            request_fingerprint: "fp-1".to_owned(),
            release_id: "rel-1".to_owned(),
            created_resources: vec![CreatedResource {
                kind: ProvisionableResourceKind::GraphShard,
                canister_id: Principal::from_slice(&[4; 29]),
                artifact_hash: "deadbeef".to_owned(),
            }],
            terminal_outcome: ProvisionResultOutcome::Installed,
        };
        let bytes = candid::Encode!(&result).expect("encode");
        let decoded = candid::Decode!(bytes.as_slice(), ProvisionResult).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn router_provision_ack_candid_roundtrip() {
        let ack = RouterProvisionAck {
            deployment_id: "deploy-1".to_owned(),
            request_id: "req-1".to_owned(),
            accepted_registry_version: 17,
        };
        let bytes = candid::Encode!(&ack).expect("encode");
        let decoded = candid::Decode!(bytes.as_slice(), RouterProvisionAck).expect("decode");
        assert_eq!(decoded, ack);
    }
    #[test]
    fn clear_request_absent_returns_not_found() {
        let s = store();
        let absent_key = ProvisioningRequestKey {
            request_id: "does-not-exist".to_owned(),
            deployment_id: "deploy-none".to_owned(),
        };
        assert_eq!(s.clear_request(&absent_key), Err(ClearError::NotFound));
    }
    #[test]
    fn list_by_graph_handles_max_unicode_request_id() {
        let s = store();
        // Request id starting with U+10FFFF followed by suffix — sorts above naive U+10FFFF sentinel.
        let mut req_sentinel = sample_request("\u{10ffff}req", "fp-sentinel");
        req_sentinel.graph_name = "graph-sentinel".to_owned();
        s.insert("deploy-sentinel", req_sentinel.clone())
            .expect("insert sentinel");
        // Normal request in the same graph.
        let mut req_normal = sample_request("normal-req", "fp-normal");
        req_normal.graph_name = "graph-sentinel".to_owned();
        req_normal.requested_resources[0].logical_resource_key = "shard-normal".to_owned();
        s.insert("deploy-sentinel", req_normal.clone())
            .expect("insert normal");
        // Same request_id in a different deployment must not leak into the result.
        let mut req_other_deploy = sample_request("\u{10ffff}req", "fp-other");
        req_other_deploy.graph_name = "graph-sentinel".to_owned();
        s.insert("deploy-other", req_other_deploy)
            .expect("insert other deploy");
        // Same request_id in a neighboring graph under the same deployment must not leak.
        let mut req_other_graph = sample_request("\u{10ffff}z", "fp-graph");
        req_other_graph.graph_name = "graph-sentinelz".to_owned();
        req_other_graph.requested_resources[0].logical_resource_key = "shard-neighbor".to_owned();
        s.insert("deploy-sentinel", req_other_graph)
            .expect("insert other graph");
        let listed = s.list_by_graph("deploy-sentinel", "graph-sentinel");
        assert_eq!(
            listed.len(),
            2,
            "U+10FFFF request_id must not be silently omitted"
        );
        assert!(listed.iter().any(|r| r.request_id == "\u{10ffff}req"));
        assert!(listed.iter().any(|r| r.request_id == "normal-req"));
        assert!(!listed.iter().any(|r| r.request_fingerprint == "fp-other"));
        assert!(!listed.iter().any(|r| r.request_fingerprint == "fp-graph"));
    }

    fn sample_awaiting(
        s: &RouterProvisioningRequestStore,
        deployment_id: &str,
        request_id: &str,
    ) -> RouterProvisioningRequest {
        let mut req = sample_request(request_id, "fp-await");
        req.state = RouterProvisioningRequestState::AwaitingAck;
        s.insert(deployment_id, req.clone())
            .expect("insert awaiting");
        req
    }

    #[test]
    fn commit_ack_advances_awaiting_to_completed_and_releases_locks() {
        let s = store();
        let deployment_id = "deploy-ack";
        let request_id = "req-ack";
        sample_awaiting(&s, deployment_id, request_id);

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        let record = s.commit_ack(&key, 7).expect("commit ack");
        assert_eq!(record.state, RouterProvisioningRequestState::Completed);
        assert_eq!(record.accepted_registry_version, Some(7));

        let intent_key = ProvisioningIntentKey::new(
            deployment_id,
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        assert!(
            !s.intent_locked(&intent_key, &owner(request_id, deployment_id, "fp-await")),
            "intent lock released after Completed"
        );
    }

    #[test]
    fn commit_ack_replay_on_completed_same_version() {
        let s = store();
        let deployment_id = "deploy-replay";
        let request_id = "req-replay";
        sample_awaiting(&s, deployment_id, request_id);

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        let first = s.commit_ack(&key, 7).expect("first ack");
        let second = s.commit_ack(&key, 7).expect("replay ack");
        assert_eq!(first, second);
    }

    #[test]
    fn commit_ack_conflict_on_completed_different_version() {
        let s = store();
        let deployment_id = "deploy-conflict";
        let request_id = "req-conflict";
        sample_awaiting(&s, deployment_id, request_id);

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        s.commit_ack(&key, 7).expect("first ack");
        let err = s
            .commit_ack(&key, 8)
            .expect_err("different version must conflict");
        assert_eq!(err, AckCommitError::Conflict { stored: 7 });
    }

    #[test]
    fn commit_ack_invalid_state_on_completed_missing_version() {
        let deployment_id = "deploy-nover";
        let request_id = "req-nover";
        let mut req = sample_request(request_id, "fp-nover");
        req.state = RouterProvisioningRequestState::Completed;
        req.accepted_registry_version = None;
        let s = store();
        s.insert(deployment_id, req)
            .expect("insert completed no version");

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        let err = s
            .commit_ack(&key, 7)
            .expect_err("missing version must be invalid state");
        assert!(
            matches!(err, AckCommitError::InvalidState(_)),
            "expected InvalidState, got {err:?}"
        );
    }

    #[test]
    fn commit_ack_invalid_state_on_non_awaiting_non_completed() {
        let deployment_id = "deploy-pending";
        let request_id = "req-pending";
        let mut req = sample_request(request_id, "fp-pending");
        req.state = RouterProvisioningRequestState::Pending;
        let s = store();
        s.insert(deployment_id, req).expect("insert pending");

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        let err = s
            .commit_ack(&key, 7)
            .expect_err("Pending must be invalid state");
        assert!(
            matches!(err, AckCommitError::InvalidState(_)),
            "expected InvalidState, got {err:?}"
        );
    }

    #[test]
    fn commit_ack_preflight_rejects_missing_locks() {
        let deployment_id = "deploy-missing-locks";
        let request_id = "req-missing-locks";
        let mut req = sample_request(request_id, "fp-missing-locks");
        req.state = RouterProvisioningRequestState::AwaitingAck;
        let s = store();
        s.insert(deployment_id, req).expect("insert awaiting");

        // Manually release the intent lock to simulate corruption.
        let intent_key = ProvisioningIntentKey::new(
            deployment_id,
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            locks.remove(&intent_key);
        });

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        let err = s
            .commit_ack(&key, 7)
            .expect_err("missing locks must be invalid state");
        assert!(
            matches!(err, AckCommitError::InvalidState(_)),
            "expected InvalidState, got {err:?}"
        );

        // The record must remain AwaitingAck; no partial write happened before the preflight.
        let record = s.get_by_request_id(&key).expect("record still exists");
        assert_eq!(record.state, RouterProvisioningRequestState::AwaitingAck);
        assert_eq!(record.accepted_registry_version, None);
    }

    #[test]
    fn commit_ack_preflight_rejects_lock_with_wrong_owner() {
        let deployment_id = "deploy-wrong-owner";
        let request_id = "req-wrong-owner";
        let mut req = sample_request(request_id, "fp-wrong-owner");
        req.state = RouterProvisioningRequestState::AwaitingAck;
        let s = store();
        s.insert(deployment_id, req).expect("insert awaiting");

        // Replace the rightful owner with an impostor that holds the same resource.
        let intent_key = ProvisioningIntentKey::new(
            deployment_id,
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        let impostor = IntentLockOwner::new(
            ProvisioningRequestKey::new("other-request", deployment_id),
            "other-fp".to_owned(),
        );
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            locks.insert(intent_key.clone(), impostor);
        });

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        let err = s
            .commit_ack(&key, 7)
            .expect_err("wrong owner must be invalid state");
        assert!(
            matches!(err, AckCommitError::InvalidState(_)),
            "expected InvalidState, got {err:?}"
        );

        // No partial write: record stays AwaitingAck, version unset.
        let record = s.get_by_request_id(&key).expect("record still exists");
        assert_eq!(record.state, RouterProvisioningRequestState::AwaitingAck);
        assert_eq!(record.accepted_registry_version, None);
    }

    #[test]
    fn release_intent_locks_owned_by_does_not_remove_foreign_lock() {
        let deployment_id = "deploy-foreign-lock";
        let request_id = "req-foreign-lock";
        let mut req = sample_request(request_id, "fp-foreign-lock");
        req.state = RouterProvisioningRequestState::AwaitingAck;
        let s = store();
        s.insert(deployment_id, req).expect("insert awaiting");

        // A foreign request holds the same resource. This can only happen in a corrupt
        // scenario because `insert` preflight rejects conflicting locks; we force it to
        // test that release is owner-scoped.
        let intent_key = ProvisioningIntentKey::new(
            deployment_id,
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        let foreign = IntentLockOwner::new(
            ProvisioningRequestKey::new("foreign-request", deployment_id),
            "foreign-fp".to_owned(),
        );
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            locks.insert(intent_key.clone(), foreign.clone());
        });

        // Releasing locks for the local record must leave the foreign lock intact.
        let local_record = s
            .get_by_request_id(&ProvisioningRequestKey::new(request_id, deployment_id))
            .expect("local record");
        s.release_intent_locks_owned_by(deployment_id, &local_record);
        assert!(
            s.intent_locked(&intent_key, &foreign),
            "foreign intent lock must survive owner-scoped release"
        );
    }

    #[test]
    fn commit_ack_not_found_for_missing_record() {
        let s = store();
        let key = ProvisioningRequestKey::new("no-such", "deploy-none");
        let err = s
            .commit_ack(&key, 7)
            .expect_err("missing record must be NotFound");
        assert!(
            matches!(err, AckCommitError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn rollback_if_inserted_and_awaiting_preserves_completed_record() {
        let s = store();
        let deployment_id = "deploy-rollback-completed";
        let request_id = "req-rollback-completed";
        let mut req = sample_request(request_id, "fp-rollback-completed");
        req.state = RouterProvisioningRequestState::AwaitingAck;
        let outcome = s.insert(deployment_id, req).expect("insert awaiting");
        assert!(matches!(outcome, InsertionOutcome::Inserted(_)));

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        s.commit_ack(&key, 7).expect("commit to completed");

        // A retry whose outbound send failed must not delete the durable Completed record.
        s.rollback_if_inserted_and_awaiting(&key, &outcome);

        let record = s.get_by_request_id(&key).expect("record still exists");
        assert_eq!(record.state, RouterProvisioningRequestState::Completed);
        assert_eq!(record.accepted_registry_version, Some(7));
    }

    #[test]
    fn rollback_if_inserted_and_awaiting_removes_fresh_awaiting_record() {
        let s = store();
        let deployment_id = "deploy-rollback-awaiting";
        let request_id = "req-rollback-awaiting";
        let mut req = sample_request(request_id, "fp-rollback-awaiting");
        req.state = RouterProvisioningRequestState::AwaitingAck;
        let outcome = s.insert(deployment_id, req).expect("insert awaiting");
        assert!(matches!(outcome, InsertionOutcome::Inserted(_)));

        let key = ProvisioningRequestKey::new(request_id, deployment_id);
        let record = s.get_by_request_id(&key).expect("record exists");
        assert_eq!(record.state, RouterProvisioningRequestState::AwaitingAck);

        // Simulate send-failure rollback via the invocation-owned guard: clear only because
        // the current operation inserted the record and it is still AwaitingAck.
        s.rollback_if_inserted_and_awaiting(&key, &outcome);
        assert!(s.get_by_request_id(&key).is_none());

        let intent_key = ProvisioningIntentKey::new(
            deployment_id,
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        assert!(
            !s.intent_locked(
                &intent_key,
                &owner(request_id, deployment_id, "fp-rollback-awaiting"),
            ),
            "intent lock released after rollback"
        );
    }

    #[test]
    fn rollback_if_inserted_and_awaiting_preserves_pre_existing_awaiting_record() {
        let s = store();
        let deployment_id = "deploy-rollback-preexisting";
        let request_id = "req-rollback-preexisting";
        let mut req = sample_request(request_id, "fp-preexisting");
        req.state = RouterProvisioningRequestState::AwaitingAck;

        // First invocation creates the AwaitingAck record and holds the locks.
        let first_outcome = s.insert(deployment_id, req.clone()).expect("first insert");
        assert!(matches!(first_outcome, InsertionOutcome::Inserted(_)));

        // Retry with the same fingerprint returns the existing record (idempotent insert).
        let second_outcome = s.insert(deployment_id, req.clone()).expect("retry insert");
        assert!(matches!(second_outcome, InsertionOutcome::Existing(_)));

        let key = ProvisioningRequestKey::new(request_id, deployment_id);

        // A transient send failure during the retry must NOT undo the prior invocation's work.
        s.rollback_if_inserted_and_awaiting(&key, &second_outcome);

        let record = s
            .get_by_request_id(&key)
            .expect("pre-existing AwaitingAck record survives");
        assert_eq!(record.state, RouterProvisioningRequestState::AwaitingAck);
        assert_eq!(record.accepted_registry_version, None);

        let intent_key = ProvisioningIntentKey::new(
            deployment_id,
            ProvisionableResourceKind::GraphShard,
            "shard-0",
        );
        assert!(
            s.intent_locked(
                &intent_key,
                &owner(request_id, deployment_id, "fp-preexisting"),
            ),
            "pre-existing intent lock remains held"
        );
    }

    #[test]
    fn router_provisioning_request_stable_roundtrip_without_accepted_version() {
        // Simulate a record encoded before accepted_registry_version existed.
        let record = RouterProvisioningRequest {
            request_id: "old-req".to_owned(),
            request_fingerprint: "old-fp".to_owned(),
            caller: Principal::anonymous(),
            graph_name: "old.graph".to_owned(),
            reserved_graph_id: None,
            requested_resources: vec![],
            state: RouterProvisioningRequestState::Pending,
            provision_receipt: None,
            accepted_registry_version: None,
            created_at_ns: 0,
        };
        let bytes = record.to_bytes();
        let decoded = RouterProvisioningRequest::from_bytes(bytes);
        assert_eq!(decoded.accepted_registry_version, None);
    }
}

mod register_graph_provisioning_guard {
    use super::*;
    use crate::init::RouterInitArgs;
    use crate::types::ProvisioningState;

    fn setup_store() -> (RouterStore, Principal) {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::from_slice(&[1; 29]),
            initial_admins: vec![],
            provision_canister: None,
        });
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        (store, admin)
    }

    #[test]
    fn provisioning_guard_blocks_pending_and_failed_but_allows_none() {
        let (store, admin) = setup_store();
        register_test_graph(&store, admin, "tenant.main");

        // None -> data-plane resolution succeeds.
        store
            .resolve_graph_id("tenant.main")
            .expect("None resolves");
        store
            .resolve_graph_id_authorized("tenant.main", admin)
            .expect("None resolves authorized");

        // Pending -> data-plane resolution rejected at every shared resolver.
        store
            .admin_set_graph_provisioning_state(
                admin,
                "tenant.main",
                ProvisioningState::Pending {
                    request_id: "r-1".into(),
                },
            )
            .expect("set pending");
        let err = store
            .resolve_graph_id("tenant.main")
            .expect_err("pending blocked");
        assert!(matches!(err, RouterError::GraphUnavailable));
        let err = store
            .resolve_graph_id_authorized("tenant.main", admin)
            .expect_err("pending blocked authorized");
        assert!(matches!(err, RouterError::GraphUnavailable));

        // Failed -> still blocked.
        store
            .admin_set_graph_provisioning_state(
                admin,
                "tenant.main",
                ProvisioningState::Failed {
                    request_id: "r-1".into(),
                    reason: "boom".into(),
                },
            )
            .expect("set failed");
        let err = store
            .resolve_graph_id("tenant.main")
            .expect_err("failed blocked");
        assert!(matches!(err, RouterError::GraphUnavailable));

        // Pending -> None transition restores resolvability (the Slice B ack path).
        store
            .admin_set_graph_provisioning_state(admin, "tenant.main", ProvisioningState::None)
            .expect("set none");
        store
            .resolve_graph_id("tenant.main")
            .expect("None resolves again");

        // Operators still see the entry while blocked (get_graph path is not gated).
        store
            .admin_set_graph_provisioning_state(
                admin,
                "tenant.main",
                ProvisioningState::Pending {
                    request_id: "r-2".into(),
                },
            )
            .expect("set pending");
        let entry = store
            .get_graph_operator("tenant.main", admin)
            .expect("operator read sees pending graph");
        assert_eq!(entry.graph_name, "tenant.main");
    }

    #[test]
    fn duplicate_graph_name_is_conflict_and_existing_state_survives() {
        let (store, admin) = setup_store();
        register_test_graph(&store, admin, "tenant.main");
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: Principal::from_slice(&[7; 29]),
                index_canister: Principal::from_slice(&[8; 29]),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register first shard");

        let dup = GraphRegistryEntry {
            graph_id: GraphId::from_raw(0),
            graph_name: "tenant.main".to_owned(),
            canister_id: Principal::from_slice(&[9; 29]),
            owner: admin,
            admins: BTreeSet::new(),
            status: GraphStatus::Active,
            version: 1,
            updated_at_ns: 0,
            provisioning_state: ProvisioningState::None,
            is_home: false,
        };
        let err = store
            .admin_register_graph(admin, dup)
            .expect_err("duplicate graph name rejected");
        assert!(matches!(err, RouterError::Conflict(_)));

        // The pre-existing entry and its shard survive unchanged.
        let entry = store
            .get_graph_operator("tenant.main", admin)
            .expect("original entry survives");
        assert_eq!(entry.canister_id, Principal::management_canister());
        assert_eq!(
            store
                .list_shards_for_graph("tenant.main")
                .expect("shards")
                .len(),
            1,
            "the original shard registration survives the rejected duplicate"
        );
    }

    #[test]
    fn shard_id_must_match_router_dense_allocation() {
        let (store, admin) = setup_store();
        register_test_graph(&store, admin, "tenant.main");

        // The Router allocates graph-local dense shard ids; a caller-chosen id that does not
        // match the next allocation is rejected, not silently accepted.
        let err = futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(3),
                graph_canister: Principal::from_slice(&[7; 29]),
                index_canister: Principal::from_slice(&[8; 29]),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect_err("wrong shard id rejected");
        assert!(matches!(err, RouterError::Conflict(_)));
        assert!(
            store
                .list_shards_for_graph("tenant.main")
                .expect("shards")
                .is_empty(),
            "no shard is registered when allocation does not match"
        );
    }
}

mod graph_summary_views {
    use super::*;
    use gleaph_graph_kernel::federation::IndexSyncStatus;

    fn setup_one_shard(store: &RouterStore, admin: Principal) {
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
    }

    #[test]
    fn list_graph_summaries_are_registry_local() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        setup_one_shard(&store, admin);
        register_test_graph(&store, admin, "tenant.b");

        let summaries = store
            .list_graph_summaries(admin)
            .expect("registry-local summaries");
        assert_eq!(summaries.len(), 2);
        let main = summaries
            .iter()
            .find(|s| s.graph_name == "tenant.main")
            .expect("tenant.main summary");
        assert_eq!(main.shard_count, 1);
        assert_eq!(main.status, GraphStatus::Active);
        assert_eq!(main.provisioning_state, ProvisioningState::None);
        let b = summaries
            .iter()
            .find(|s| s.graph_name == "tenant.b")
            .expect("tenant.b summary");
        assert_eq!(b.shard_count, 0);
    }

    #[test]
    fn graph_health_view_is_best_effort_with_notes_and_summary() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        setup_one_shard(&store, admin);

        // Native builds: the per-shard status forward fails closed -> an unreachable note, not an
        // error, and the summary rows are still populated.
        let health = futures::executor::block_on(store.graph_health_view(
            admin,
            "tenant.main",
            crate::graph_client::index_sync_status,
        ))
        .expect("health view is best-effort");
        assert_eq!(health.graph.graph_name, "tenant.main");
        assert_eq!(health.graph.shard_count, 1);
        assert_eq!(health.reachable_shard_count, 0);
        assert!(!health.index_sync_converged);
        assert_eq!(health.vector_index_count, 0);
        assert!(health.unhealthy_vector_indexes.is_empty());
        assert_eq!(health.notes.len(), 1);
        assert!(health.notes[0].contains("unreachable"));

        // A converging forward reports reachable + converged with no notes.
        let health =
            futures::executor::block_on(store.graph_health_view(admin, "tenant.main", |_| async {
                Ok(IndexSyncStatus::new(0, 0))
            }))
            .expect("health view with converging shard");
        assert_eq!(health.reachable_shard_count, 1);
        assert!(health.index_sync_converged);
        assert!(health.notes.is_empty());
    }
}
