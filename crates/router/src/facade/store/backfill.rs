//! Posting backfill domain: router-stable cursors and shard orchestration.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::federation::{
    BackfillShardState, EdgeBackfillShardState, EdgePostingBackfillArgs, EdgePostingBackfillResult,
    GraphShardKey, PostingBackfillArgs, PostingBackfillResult, ShardId, ShardRegistryEntry,
};

use super::super::stable::graph_catalog::lookup_graph_id;

use super::super::stable::ROUTER_EDGE_BACKFILL_STATE;
use super::super::stable::ROUTER_LABEL_BACKFILL_STATE;
use super::super::stable::ROUTER_VERTEX_PROPERTY_BACKFILL_STATE;
use super::RouterStore;
use crate::facade::auth;
use crate::state::RouterError;
use crate::types::{
    AdminEdgeBackfillStepArgs, AdminEdgeBackfillStepResult, AdminLabelBackfillStepArgs,
    AdminLabelBackfillStepResult, AdminVertexPropertyBackfillStepArgs,
    AdminVertexPropertyBackfillStepResult, EdgeBackfillShardStatus, LabelBackfillShardStatus,
    VertexPropertyBackfillShardStatus,
};

impl RouterStore {
    pub(crate) async fn admin_label_backfill_step<F, Fut>(
        &self,
        caller: Principal,
        args: AdminLabelBackfillStepArgs,
        call_backfill: F,
    ) -> Result<AdminLabelBackfillStepResult, RouterError>
    where
        F: FnOnce(Principal, PostingBackfillArgs) -> Fut,
        Fut: Future<Output = Result<PostingBackfillResult, String>>,
    {
        auth::require_admin(&caller)?;
        if args.max_vertices == 0 {
            return Err(RouterError::InvalidArgument(
                "max_vertices must be greater than zero".into(),
            ));
        }

        let shard = self.resolve_shard_for_backfill(&args.logical_graph_name, args.shard_id)?;
        let cursor_key = GraphShardKey::new(shard.graph_id, args.shard_id);
        let mut cursor = self.load_label_backfill_state(cursor_key);
        if cursor.done {
            return Ok(AdminLabelBackfillStepResult {
                shard_id: args.shard_id,
                next_vertex_id: cursor.next_vertex_id,
                vertices_processed: 0,
                postings_synced: 0,
                done: true,
            });
        }

        let result = call_backfill(
            shard.graph_canister,
            PostingBackfillArgs {
                start_vertex_id: cursor.next_vertex_id,
                max_vertices: args.max_vertices,
            },
        )
        .await
        .map_err(RouterError::Internal)?;
        cursor.apply_batch_progress(result.next_vertex_id, result.done);
        self.store_label_backfill_state(cursor_key, cursor);

        Ok(AdminLabelBackfillStepResult {
            shard_id: args.shard_id,
            next_vertex_id: result.next_vertex_id,
            vertices_processed: result.vertices_processed,
            postings_synced: result.postings_synced,
            done: result.done,
        })
    }

    pub(crate) fn admin_list_label_backfill_status(
        &self,
        caller: Principal,
        logical_graph_name: &str,
    ) -> Result<Vec<LabelBackfillShardStatus>, RouterError> {
        auth::require_admin(&caller)?;
        let shards = self.list_live_shards_for_graph(logical_graph_name)?;
        let mut out: Vec<LabelBackfillShardStatus> = shards
            .into_iter()
            .map(|shard| {
                let cursor_key = GraphShardKey::new(shard.graph_id, shard.shard_id);
                let cursor = self.load_label_backfill_state(cursor_key);
                LabelBackfillShardStatus {
                    shard_id: shard.shard_id,
                    next_vertex_id: cursor.next_vertex_id,
                    done: cursor.done,
                }
            })
            .collect();
        out.sort_by_key(|status| status.shard_id);
        Ok(out)
    }

    fn load_label_backfill_state(&self, key: GraphShardKey) -> BackfillShardState {
        ROUTER_LABEL_BACKFILL_STATE.with_borrow(|state| state.get(&key).unwrap_or_default())
    }

    fn store_label_backfill_state(&self, key: GraphShardKey, cursor: BackfillShardState) {
        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(key, cursor);
        });
    }

    pub(crate) async fn admin_vertex_property_backfill_step<F, Fut>(
        &self,
        caller: Principal,
        args: AdminVertexPropertyBackfillStepArgs,
        call_backfill: F,
    ) -> Result<AdminVertexPropertyBackfillStepResult, RouterError>
    where
        F: FnOnce(Principal, PostingBackfillArgs) -> Fut,
        Fut: Future<Output = Result<PostingBackfillResult, String>>,
    {
        auth::require_admin(&caller)?;
        if args.max_vertices == 0 {
            return Err(RouterError::InvalidArgument(
                "max_vertices must be greater than zero".into(),
            ));
        }

        let shard = self.resolve_shard_for_backfill(&args.logical_graph_name, args.shard_id)?;
        let cursor_key = GraphShardKey::new(shard.graph_id, args.shard_id);
        let mut cursor = self.load_vertex_property_backfill_state(cursor_key);
        if cursor.done {
            return Ok(AdminVertexPropertyBackfillStepResult {
                shard_id: args.shard_id,
                next_vertex_id: cursor.next_vertex_id,
                vertices_processed: 0,
                postings_synced: 0,
                done: true,
            });
        }

        let result = call_backfill(
            shard.graph_canister,
            PostingBackfillArgs {
                start_vertex_id: cursor.next_vertex_id,
                max_vertices: args.max_vertices,
            },
        )
        .await
        .map_err(RouterError::Internal)?;
        cursor.apply_batch_progress(result.next_vertex_id, result.done);
        self.store_vertex_property_backfill_state(cursor_key, cursor);

        Ok(AdminVertexPropertyBackfillStepResult {
            shard_id: args.shard_id,
            next_vertex_id: result.next_vertex_id,
            vertices_processed: result.vertices_processed,
            postings_synced: result.postings_synced,
            done: result.done,
        })
    }

    pub(crate) fn admin_list_vertex_property_backfill_status(
        &self,
        caller: Principal,
        logical_graph_name: &str,
    ) -> Result<Vec<VertexPropertyBackfillShardStatus>, RouterError> {
        auth::require_admin(&caller)?;
        let shards = self.list_live_shards_for_graph(logical_graph_name)?;
        let mut out: Vec<VertexPropertyBackfillShardStatus> = shards
            .into_iter()
            .map(|shard| {
                let cursor_key = GraphShardKey::new(shard.graph_id, shard.shard_id);
                let cursor = self.load_vertex_property_backfill_state(cursor_key);
                VertexPropertyBackfillShardStatus {
                    shard_id: shard.shard_id,
                    next_vertex_id: cursor.next_vertex_id,
                    done: cursor.done,
                }
            })
            .collect();
        out.sort_by_key(|status| status.shard_id);
        Ok(out)
    }

    fn load_vertex_property_backfill_state(&self, key: GraphShardKey) -> BackfillShardState {
        ROUTER_VERTEX_PROPERTY_BACKFILL_STATE
            .with_borrow(|state| state.get(&key).unwrap_or_default())
    }

    fn store_vertex_property_backfill_state(&self, key: GraphShardKey, cursor: BackfillShardState) {
        ROUTER_VERTEX_PROPERTY_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(key, cursor);
        });
    }

    pub(crate) async fn admin_edge_backfill_step<F, Fut>(
        &self,
        caller: Principal,
        args: AdminEdgeBackfillStepArgs,
        call_backfill: F,
    ) -> Result<AdminEdgeBackfillStepResult, RouterError>
    where
        F: FnOnce(Principal, EdgePostingBackfillArgs) -> Fut,
        Fut: Future<Output = Result<EdgePostingBackfillResult, String>>,
    {
        auth::require_admin(&caller)?;
        if args.max_entries == 0 {
            return Err(RouterError::InvalidArgument(
                "max_entries must be greater than zero".into(),
            ));
        }

        let shard = self.resolve_shard_for_backfill(&args.logical_graph_name, args.shard_id)?;
        let cursor_key = GraphShardKey::new(shard.graph_id, args.shard_id);
        let mut cursor = self.load_edge_backfill_state(cursor_key);
        if cursor.done {
            return Ok(AdminEdgeBackfillStepResult {
                shard_id: args.shard_id,
                next_after_key: cursor.after_key,
                entries_processed: 0,
                postings_synced: 0,
                done: true,
            });
        }

        let result = call_backfill(
            shard.graph_canister,
            EdgePostingBackfillArgs {
                after_key: cursor.after_key.clone(),
                max_entries: args.max_entries,
            },
        )
        .await
        .map_err(RouterError::Internal)?;
        cursor.apply_batch_progress(result.next_after_key.clone(), result.done);
        self.store_edge_backfill_state(cursor_key, cursor);

        Ok(AdminEdgeBackfillStepResult {
            shard_id: args.shard_id,
            next_after_key: result.next_after_key,
            entries_processed: result.entries_processed,
            postings_synced: result.postings_synced,
            done: result.done,
        })
    }

    pub(crate) fn admin_list_edge_backfill_status(
        &self,
        caller: Principal,
        logical_graph_name: &str,
    ) -> Result<Vec<EdgeBackfillShardStatus>, RouterError> {
        auth::require_admin(&caller)?;
        let shards = self.list_live_shards_for_graph(logical_graph_name)?;
        let mut out: Vec<EdgeBackfillShardStatus> = shards
            .into_iter()
            .map(|shard| {
                let cursor_key = GraphShardKey::new(shard.graph_id, shard.shard_id);
                let cursor = self.load_edge_backfill_state(cursor_key);
                EdgeBackfillShardStatus {
                    shard_id: shard.shard_id,
                    after_key: cursor.after_key,
                    done: cursor.done,
                }
            })
            .collect();
        out.sort_by_key(|status| status.shard_id);
        Ok(out)
    }

    fn load_edge_backfill_state(&self, key: GraphShardKey) -> EdgeBackfillShardState {
        ROUTER_EDGE_BACKFILL_STATE.with_borrow(|state| state.get(&key).unwrap_or_default())
    }

    fn store_edge_backfill_state(&self, key: GraphShardKey, cursor: EdgeBackfillShardState) {
        ROUTER_EDGE_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(key, cursor);
        });
    }

    fn resolve_shard_for_backfill(
        &self,
        logical_graph_name: &str,
        shard_id: ShardId,
    ) -> Result<ShardRegistryEntry, RouterError> {
        let graph_id = lookup_graph_id(logical_graph_name)
            .ok_or_else(|| RouterError::NotFound(logical_graph_name.to_owned()))?;
        let entry = self.resolve_shard(graph_id, shard_id)?;
        if !entry.index_attached {
            return Err(RouterError::InvalidArgument(format!(
                "shard {shard_id:?} for `{logical_graph_name}` is not index-attached"
            )));
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::stable::graph_catalog::lookup_graph_id;
    use super::*;
    use crate::init::RouterInitArgs;
    use crate::types::{
        AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState,
    };
    use gleaph_graph_kernel::entry::GraphId;
    use std::collections::BTreeSet;

    fn test_init_args() -> RouterInitArgs {
        RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
        }
    }

    fn graph_principal(n: u8) -> Principal {
        Principal::from_slice(&[n])
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
    }

    #[test]
    fn apply_batch_progress_updates_cursor() {
        let mut cursor = BackfillShardState::default();
        cursor.apply_batch_progress(42, false);
        assert_eq!(cursor.next_vertex_id, 42);
        assert!(!cursor.done);
        cursor.apply_batch_progress(100, true);
        assert_eq!(cursor.next_vertex_id, 100);
        assert!(cursor.done);
    }

    #[test]
    fn admin_label_backfill_step_advances_cursor() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");

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
        .expect("register shard");

        let result = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 32,
            },
            |_graph, args| async move {
                Ok(PostingBackfillResult {
                    next_vertex_id: args.start_vertex_id.saturating_add(args.max_vertices),
                    vertices_processed: args.max_vertices,
                    postings_synced: 5,
                    done: false,
                })
            },
        ))
        .expect("step");

        assert_eq!(result.shard_id, ShardId::new(0));
        assert_eq!(result.vertices_processed, 32);
        assert_eq!(result.postings_synced, 5);
        assert!(!result.done);

        let status = store
            .admin_list_label_backfill_status(admin, "tenant.main")
            .expect("status");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].next_vertex_id, 32);
        assert!(!status[0].done);
    }

    #[test]
    fn admin_label_backfill_step_rejects_wrong_graph() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
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
        .expect("register shard");

        let err = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "other.graph".into(),
                shard_id: ShardId::new(0),
                max_vertices: 1,
            },
            |_graph, _args| async { unreachable!() },
        ))
        .expect_err("wrong graph");

        assert!(matches!(err, RouterError::ShardNotRegistered));
    }

    #[test]
    fn done_shard_step_is_idempotent() {
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

        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|map| {
            let graph_id = lookup_graph_id("tenant.main").expect("graph id");
            map.insert(
                GraphShardKey::new(graph_id, ShardId::new(0)),
                BackfillShardState {
                    next_vertex_id: 99,
                    done: true,
                },
            );
        });

        let result = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 16,
            },
            |_graph, _args| async { unreachable!() },
        ))
        .expect("step");

        assert!(result.done);
        assert_eq!(result.next_vertex_id, 99);
        assert_eq!(result.vertices_processed, 0);
    }

    #[test]
    fn admin_vertex_property_backfill_step_advances_cursor() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");

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
        .expect("register shard");

        let result = futures::executor::block_on(store.admin_vertex_property_backfill_step(
            admin,
            AdminVertexPropertyBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 32,
            },
            |_graph, args| async move {
                Ok(PostingBackfillResult {
                    next_vertex_id: args.start_vertex_id.saturating_add(args.max_vertices),
                    vertices_processed: args.max_vertices,
                    postings_synced: 5,
                    done: false,
                })
            },
        ))
        .expect("step");

        assert_eq!(result.shard_id, ShardId::new(0));
        assert_eq!(result.vertices_processed, 32);
        assert_eq!(result.postings_synced, 5);
        assert!(!result.done);

        let status = store
            .admin_list_vertex_property_backfill_status(admin, "tenant.main")
            .expect("status");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].next_vertex_id, 32);
        assert!(!status[0].done);
    }

    #[test]
    fn admin_edge_backfill_step_advances_cursor() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");

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
        .expect("register shard");

        let key = vec![7u8; gleaph_graph_kernel::federation::EDGE_PROPERTY_KEY_BYTES];
        let expected_key = key.clone();
        let result = futures::executor::block_on(store.admin_edge_backfill_step(
            admin,
            AdminEdgeBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_entries: 32,
            },
            |_graph, args| async move {
                Ok(EdgePostingBackfillResult {
                    next_after_key: Some(key.clone()),
                    entries_processed: args.max_entries,
                    postings_synced: 5,
                    done: false,
                })
            },
        ))
        .expect("step");

        assert_eq!(result.shard_id, ShardId::new(0));
        assert_eq!(result.entries_processed, 32);
        assert_eq!(result.postings_synced, 5);
        assert_eq!(result.next_after_key, Some(expected_key));
        assert!(!result.done);

        let status = store
            .admin_list_edge_backfill_status(admin, "tenant.main")
            .expect("status");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].after_key, Some(vec![7u8; 14]));
        assert!(!status[0].done);
    }

    #[test]
    fn backfill_cursors_isolated_per_graph_same_shard_ordinal() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "graph_a");
        register_test_graph(&store, admin, "graph_b");

        for (name, graph_byte) in [("graph_a", 21u8), ("graph_b", 31u8)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id: ShardId::new(0),
                    graph_canister: graph_principal(graph_byte),
                    index_canister: graph_principal(graph_byte + 1),
                    logical_graph_name: name.into(),
                },
            ))
            .expect("register shard");
        }

        let graph_a = lookup_graph_id("graph_a").expect("graph a");
        let _graph_b = lookup_graph_id("graph_b").expect("graph b");
        let shard_id = ShardId::new(0);

        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(
                GraphShardKey::new(graph_a, shard_id),
                BackfillShardState {
                    next_vertex_id: 99,
                    done: true,
                },
            );
        });
        ROUTER_VERTEX_PROPERTY_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(
                GraphShardKey::new(graph_a, shard_id),
                BackfillShardState {
                    next_vertex_id: 50,
                    done: true,
                },
            );
        });
        ROUTER_EDGE_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(
                GraphShardKey::new(graph_a, shard_id),
                EdgeBackfillShardState {
                    after_key: Some(vec![
                        7u8;
                        gleaph_graph_kernel::federation::EDGE_PROPERTY_KEY_BYTES
                    ]),
                    done: true,
                },
            );
        });

        let label_a = store
            .admin_list_label_backfill_status(admin, "graph_a")
            .expect("label a")[0]
            .done;
        let label_b = store
            .admin_list_label_backfill_status(admin, "graph_b")
            .expect("label b")[0]
            .done;
        assert!(label_a);
        assert!(!label_b);

        let prop_a = store
            .admin_list_vertex_property_backfill_status(admin, "graph_a")
            .expect("prop a")[0]
            .done;
        let prop_b = store
            .admin_list_vertex_property_backfill_status(admin, "graph_b")
            .expect("prop b")[0]
            .done;
        assert!(prop_a);
        assert!(!prop_b);

        let edge_a = store
            .admin_list_edge_backfill_status(admin, "graph_a")
            .expect("edge a")[0]
            .done;
        let edge_b = store
            .admin_list_edge_backfill_status(admin, "graph_b")
            .expect("edge b")[0]
            .done;
        assert!(edge_a);
        assert!(!edge_b);

        let result_b = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "graph_b".into(),
                shard_id,
                max_vertices: 8,
            },
            |_graph, args| async move {
                Ok(PostingBackfillResult {
                    next_vertex_id: args.start_vertex_id.saturating_add(args.max_vertices),
                    vertices_processed: args.max_vertices,
                    postings_synced: 1,
                    done: false,
                })
            },
        ))
        .expect("graph b backfill should run");
        assert!(!result_b.done);
        assert_eq!(result_b.vertices_processed, 8);
    }

    #[test]
    fn admin_label_backfill_step_rejects_pending_shard() {
        use crate::facade::stable::ROUTER_SHARDS;
        use gleaph_graph_kernel::federation::GraphShardKey;

        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: graph_principal(21),
                index_canister: graph_principal(22),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register shard");

        let graph_id = lookup_graph_id("tenant.main").expect("graph id");
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let key = GraphShardKey::new(graph_id, ShardId::new(0));
            let mut entry = shards.get(&key).expect("shard row");
            entry.index_attached = false;
            shards.insert(key, entry);
        });

        let err = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 8,
            },
            |_graph, _args| async { unreachable!() },
        ))
        .expect_err("pending shard must not backfill");

        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }
}
