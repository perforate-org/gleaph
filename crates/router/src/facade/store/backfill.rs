//! Posting backfill domain: router-stable cursors and shard orchestration.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::federation::{
    BackfillShardState, PostingBackfillArgs, PostingBackfillResult, ShardId, ShardRegistryEntry,
};

use super::super::stable::graph_catalog::lookup_graph_id;

use super::super::stable::ROUTER_LABEL_BACKFILL_STATE;
use super::super::stable::ROUTER_PROPERTY_BACKFILL_STATE;
use super::RouterStore;
use crate::state::RouterError;
use crate::types::{
    AdminLabelBackfillStepArgs, AdminLabelBackfillStepResult, AdminPropertyBackfillStepArgs,
    AdminPropertyBackfillStepResult, LabelBackfillShardStatus, PropertyBackfillShardStatus,
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
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        if args.max_vertices == 0 {
            return Err(RouterError::InvalidArgument(
                "max_vertices must be greater than zero".into(),
            ));
        }

        let shard = self.resolve_shard_for_backfill(&args.logical_graph_name, args.shard_id)?;
        let mut cursor = self.load_label_backfill_state(args.shard_id);
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
        self.store_label_backfill_state(args.shard_id, cursor);

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
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        let shards = self.list_shards_for_graph(logical_graph_name)?;
        let mut out: Vec<LabelBackfillShardStatus> = shards
            .into_iter()
            .map(|shard| {
                let cursor = self.load_label_backfill_state(shard.shard_id);
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

    fn load_label_backfill_state(&self, shard_id: ShardId) -> BackfillShardState {
        ROUTER_LABEL_BACKFILL_STATE.with_borrow(|state| state.get(&shard_id).unwrap_or_default())
    }

    fn store_label_backfill_state(&self, shard_id: ShardId, cursor: BackfillShardState) {
        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(shard_id, cursor);
        });
    }

    pub(crate) async fn admin_property_backfill_step<F, Fut>(
        &self,
        caller: Principal,
        args: AdminPropertyBackfillStepArgs,
        call_backfill: F,
    ) -> Result<AdminPropertyBackfillStepResult, RouterError>
    where
        F: FnOnce(Principal, PostingBackfillArgs) -> Fut,
        Fut: Future<Output = Result<PostingBackfillResult, String>>,
    {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        if args.max_vertices == 0 {
            return Err(RouterError::InvalidArgument(
                "max_vertices must be greater than zero".into(),
            ));
        }

        let shard = self.resolve_shard_for_backfill(&args.logical_graph_name, args.shard_id)?;
        let mut cursor = self.load_property_backfill_state(args.shard_id);
        if cursor.done {
            return Ok(AdminPropertyBackfillStepResult {
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
        self.store_property_backfill_state(args.shard_id, cursor);

        Ok(AdminPropertyBackfillStepResult {
            shard_id: args.shard_id,
            next_vertex_id: result.next_vertex_id,
            vertices_processed: result.vertices_processed,
            postings_synced: result.postings_synced,
            done: result.done,
        })
    }

    pub(crate) fn admin_list_property_backfill_status(
        &self,
        caller: Principal,
        logical_graph_name: &str,
    ) -> Result<Vec<PropertyBackfillShardStatus>, RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        let shards = self.list_shards_for_graph(logical_graph_name)?;
        let mut out: Vec<PropertyBackfillShardStatus> = shards
            .into_iter()
            .map(|shard| {
                let cursor = self.load_property_backfill_state(shard.shard_id);
                PropertyBackfillShardStatus {
                    shard_id: shard.shard_id,
                    next_vertex_id: cursor.next_vertex_id,
                    done: cursor.done,
                }
            })
            .collect();
        out.sort_by_key(|status| status.shard_id);
        Ok(out)
    }

    fn load_property_backfill_state(&self, shard_id: ShardId) -> BackfillShardState {
        ROUTER_PROPERTY_BACKFILL_STATE.with_borrow(|state| state.get(&shard_id).unwrap_or_default())
    }

    fn store_property_backfill_state(&self, shard_id: ShardId, cursor: BackfillShardState) {
        ROUTER_PROPERTY_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(shard_id, cursor);
        });
    }

    fn resolve_shard_for_backfill(
        &self,
        logical_graph_name: &str,
        shard_id: ShardId,
    ) -> Result<ShardRegistryEntry, RouterError> {
        let graph_id = lookup_graph_id(logical_graph_name)
            .ok_or_else(|| RouterError::NotFound(logical_graph_name.to_owned()))?;
        let entry = self.resolve_shard(shard_id)?;
        if entry.graph_id != graph_id {
            return Err(RouterError::InvalidArgument(format!(
                "shard {shard_id} is registered for graph {}, not {logical_graph_name}",
                entry.graph_id
            )));
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
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
            controllers: vec![],
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
        store.bootstrap_controllers(&[admin]);
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
        store.bootstrap_controllers(&[admin]);
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

        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }

    #[test]
    fn done_shard_step_is_idempotent() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
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
            map.insert(
                ShardId::new(0),
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
    fn admin_property_backfill_step_advances_cursor() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
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

        let result = futures::executor::block_on(store.admin_property_backfill_step(
            admin,
            AdminPropertyBackfillStepArgs {
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
            .admin_list_property_backfill_status(admin, "tenant.main")
            .expect("status");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].next_vertex_id, 32);
        assert!(!status[0].done);
    }
}
