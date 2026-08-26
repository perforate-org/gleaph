//! Single-shard routing: one registry entry, local hits only.

use gleaph_graph_kernel::federation::ShardRegistryEntry;

use crate::facade::store::RouterStore;
use crate::federation::dispatch::SeedHits;
use crate::federation::{SeedRouting, ShardingPolicy};
use crate::seed::IndexAnchor;
use crate::state::RouterError;
use gleaph_graph_kernel::entry::GraphId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandaloneSharding;

impl ShardingPolicy for StandaloneSharding {
    fn resolve_without_anchor(
        &self,
        shards: &[ShardRegistryEntry],
    ) -> Result<Vec<SeedRouting>, RouterError> {
        let routing = resolve_standalone_routing(shards)?;
        Ok(vec![routing])
    }

    fn resolve_with_hits(
        &self,
        _store: &RouterStore,
        _graph_id: GraphId,
        shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: SeedHits,
    ) -> Result<Vec<SeedRouting>, RouterError> {
        let entry = single_shard_entry(shards)?;
        let shard_hits = match hits {
            SeedHits::Vertices(hits) => SeedHits::Vertices(
                hits.into_iter()
                    .filter(|hit| hit.shard_id == entry.shard_id)
                    .collect(),
            ),
            SeedHits::Edges(hits) => SeedHits::Edges(
                hits.into_iter()
                    .filter(|hit| hit.shard_id == entry.shard_id)
                    .collect(),
            ),
        };
        Ok(vec![SeedRouting {
            shard_id: entry.shard_id,
            graph_canister: entry.graph_canister,
            hits: shard_hits,
            anchor: Some(anchor),
        }])
    }
}

fn single_shard_entry(shards: &[ShardRegistryEntry]) -> Result<&ShardRegistryEntry, RouterError> {
    shards.first().filter(|_| shards.len() == 1).ok_or_else(|| {
        RouterError::InvalidArgument("no index anchor: single-shard graph required".into())
    })
}

fn resolve_standalone_routing(shards: &[ShardRegistryEntry]) -> Result<SeedRouting, RouterError> {
    let entry = single_shard_entry(shards)?;
    Ok(SeedRouting {
        shard_id: entry.shard_id,
        graph_canister: entry.graph_canister,
        hits: SeedHits::Vertices(Vec::new()),
        anchor: None,
    })
}

#[cfg(test)]
mod tests {
    use candid::{Decode, Principal};

    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::PostingHit;

    use super::*;
    use crate::facade::stable::graph_catalog::lookup_graph_id;
    use crate::federation::ShardingPolicy;
    use crate::init::RouterInitArgs;
    use crate::types::{
        AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState,
    };
    use gleaph_graph_kernel::entry::GraphId;
    use std::collections::BTreeSet;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn register_test_graph(store: &RouterStore, admin: Principal, name: &str) {
        store
            .admin_register_graph(
                admin,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    canister_id: Principal::management_canister(),
                    owner: admin,
                    admins: BTreeSet::new(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
                name,
            )
            .expect("register graph");
    }

    fn store_with_single_shard() -> (RouterStore, ShardRegistryEntry) {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        let entry = AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        };
        futures::executor::block_on(store.admin_register_shard(admin, entry.clone()))
            .expect("register shard");
        let shards = store
            .list_shards_for_graph("tenant.main")
            .expect("list shards");
        assert_eq!(shards.len(), 1);
        (store, shards[0].clone())
    }

    #[test]
    fn without_anchor_requires_single_shard() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        for (shard_id, graph_byte) in [(ShardId::new(0), 1u8), (ShardId::new(1), 4)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph_principal(graph_byte),
                    index_canister: graph_principal(2),
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register shard");
        }
        let shards = store
            .list_shards_for_graph("tenant.main")
            .expect("list shards");
        let err = StandaloneSharding
            .resolve_without_anchor(&shards)
            .expect_err("multi-shard without anchor");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }

    #[test]
    fn without_anchor_returns_local_shard() {
        let (store, entry) = store_with_single_shard();
        let shards = store.list_shards_for_graph("tenant.main").expect("shards");
        let routings = StandaloneSharding
            .resolve_without_anchor(&shards)
            .expect("route");
        assert_eq!(routings.len(), 1);
        assert_eq!(routings[0].shard_id, entry.shard_id);
        assert_eq!(routings[0].graph_canister, entry.graph_canister);
        assert!(routings[0].hits.is_empty());
        assert!(matches!(routings[0].hits, SeedHits::Vertices(_)));
        assert!(routings[0].anchor.is_none());
    }

    #[test]
    fn with_hits_keeps_local_shard_postings_only() {
        let (store, entry) = store_with_single_shard();
        let shards = store.list_shards_for_graph("tenant.main").expect("shards");
        let anchor = IndexAnchor::Equal(crate::seed::SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId::new(1)
                .expect("physical id"),
            payload_bytes: vec![1, 2, 3],
        });
        let hits = vec![
            PostingHit {
                shard_id: entry.shard_id,
                vertex_id: 10,
            },
            PostingHit {
                shard_id: ShardId::new(99),
                vertex_id: 20,
            },
        ];
        let routings = StandaloneSharding
            .resolve_with_hits(
                &store,
                lookup_graph_id("tenant.main").expect("tenant.main"),
                &shards,
                anchor,
                SeedHits::Vertices(hits),
            )
            .expect("route");
        assert_eq!(routings.len(), 1);
        let SeedHits::Vertices(shard_hits) = &routings[0].hits else {
            panic!("expected vertex hits");
        };
        assert_eq!(shard_hits.len(), 1);
        assert_eq!(shard_hits[0].vertex_id, 10);
    }

    #[test]
    fn with_hits_encodes_seed_bindings_blob_for_single_shard_dispatch() {
        // Plan 0310 S1 seeding-parity evidence: reads funnel through the shared
        // SeedAnchorSet scan regardless of shard count, and on a single-shard graph
        // StandaloneSharding keeps the routing anchor so routings_to_dispatches encodes
        // the same per-shard seed_bindings_blob the federated path encodes.
        let (store, entry) = store_with_single_shard();
        let shards = store.list_shards_for_graph("tenant.main").expect("shards");
        let anchor = IndexAnchor::Equal(crate::seed::SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId::new(1)
                .expect("physical id"),
            payload_bytes: vec![1, 2, 3],
        });
        let hits = vec![
            PostingHit {
                shard_id: entry.shard_id,
                vertex_id: 10,
            },
            PostingHit {
                shard_id: ShardId::new(99),
                vertex_id: 20,
            },
        ];
        let routings = StandaloneSharding
            .resolve_with_hits(
                &store,
                lookup_graph_id("tenant.main").expect("tenant.main"),
                &shards,
                anchor,
                SeedHits::Vertices(hits),
            )
            .expect("route");
        let mut dispatches = crate::federation::routings_to_dispatches(routings);
        assert_eq!(dispatches.len(), 1);
        let blob = dispatches
            .remove(0)
            .seed_bindings_blob
            .expect("single-shard dispatch must carry seeds like the federated path");
        let seeds: gleaph_graph_kernel::plan_exec::SeedBindingsWire =
            candid::Decode!(&blob, gleaph_graph_kernel::plan_exec::SeedBindingsWire)
                .expect("decode seeds");
        assert_eq!(seeds.entries.len(), 1);
        assert_eq!(seeds.entries[0].variable, "u");
        assert_eq!(seeds.entries[0].local_vertex_ids, vec![10]);
    }

    /// Two leading labeled scans — the shape ANY SHORTEST between two labeled endpoints
    /// always plans — as a synthetic physical plan for `SeedAnchorSet::from_plans`.
    fn two_labeled_scan_plan() -> gleaph_gql_planner::PhysicalPlan {
        use gleaph_gql_planner::plan::PlanOp;
        use std::rc::Rc;
        let labeled_scan = |variable: &str| PlanOp::NodeScan {
            variable: Rc::from(variable),
            label: Some(gleaph_gql_planner::NodeLabelRef {
                name: Rc::from("Person"),
            }),
            property_projection: None,
        };
        gleaph_gql_planner::PhysicalPlan::from_ops(vec![labeled_scan("a"), labeled_scan("b")])
    }

    #[test]
    fn from_plans_keeps_multi_variable_label_only_prefix_on_standalone() {
        // W1 (plan 0311): the ADR 0046 label-only multi-variable fallback must not fire on
        // a single-shard topology — label anchors resolve through existing label postings
        // and the sole shard runs the remaining scans locally.
        let (store, _entry) = store_with_single_shard();
        store
            .admin_intern_vertex_label(Principal::from_slice(&[1; 29]), "tenant.main", "Person")
            .expect("intern Person");
        let graph_id = lookup_graph_id("tenant.main").expect("tenant.main");
        let stats = crate::planner_stats::RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
        );
        let set = crate::seed::SeedAnchorSet::from_plans(
            &[two_labeled_scan_plan()],
            &std::collections::BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("extract anchors")
        .expect("standalone keeps multi-variable label-only prefixes");
        assert_eq!(set.variables.len(), 2, "both endpoints anchor");
    }

    #[test]
    fn from_plans_multi_shard_still_drops_label_only_multi_variable_prefix() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        for (shard_id, graph_byte) in [(ShardId::new(0), 1u8), (ShardId::new(1), 4)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph_principal(graph_byte),
                    index_canister: graph_principal(2),
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register shard");
        }
        store
            .admin_intern_vertex_label(admin, "tenant.main", "Person")
            .expect("intern Person");
        let graph_id = lookup_graph_id("tenant.main").expect("tenant.main");
        let stats = crate::planner_stats::RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
        );
        // Multi-shard: the label-only multi-variable prefix stays fail-closed None.
        let extracted = crate::seed::SeedAnchorSet::from_plans(
            &[two_labeled_scan_plan()],
            &std::collections::BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("extract anchors");
        assert!(extracted.is_none(), "multi-shard keeps the None branch");
        // Single-variable label-only prefixes are unaffected by the topology gate.
        let single = gleaph_gql_planner::PhysicalPlan::from_ops(vec![
            gleaph_gql_planner::plan::PlanOp::NodeScan {
                variable: std::rc::Rc::from("a"),
                label: Some(gleaph_gql_planner::NodeLabelRef {
                    name: std::rc::Rc::from("Person"),
                }),
                property_projection: None,
            },
        ]);
        assert!(
            crate::seed::SeedAnchorSet::from_plans(
                &[single],
                &std::collections::BTreeMap::new(),
                &store,
                &stats
            )
            .expect("extract anchors")
            .is_some(),
            "single-variable extraction is unchanged"
        );
    }
}
