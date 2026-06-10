//! Label posting backfill domain: router-stable cursors and shard orchestration.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::federation::{
    LabelPostingBackfillArgs, LabelPostingBackfillResult, ShardRegistryEntry,
};

use super::super::stable::ROUTER_LABEL_BACKFILL_STATE;
use super::super::stable::label_backfill::LabelBackfillShardState;
use super::RouterStore;
use crate::state::RouterError;
use crate::types::{
    AdminLabelBackfillStepArgs, AdminLabelBackfillStepResult, LabelBackfillShardStatus,
};

impl RouterStore {
    pub(crate) async fn admin_label_backfill_step<F, Fut>(
        &self,
        caller: Principal,
        args: AdminLabelBackfillStepArgs,
        call_backfill: F,
    ) -> Result<AdminLabelBackfillStepResult, RouterError>
    where
        F: FnOnce(Principal, LabelPostingBackfillArgs) -> Fut,
        Fut: Future<Output = Result<LabelPostingBackfillResult, String>>,
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

        let backfill_args = build_backfill_args(&cursor, args.max_vertices);
        let result = call_backfill(shard.graph_canister, backfill_args)
            .await
            .map_err(RouterError::Internal)?;
        advance_backfill_cursor(&mut cursor, &result);
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

    fn load_label_backfill_state(&self, shard_id: u32) -> LabelBackfillShardState {
        ROUTER_LABEL_BACKFILL_STATE.with_borrow(|state| {
            state
                .get(&shard_id)
                .unwrap_or(LabelBackfillShardState::default())
        })
    }

    fn store_label_backfill_state(&self, shard_id: u32, cursor: LabelBackfillShardState) {
        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(shard_id, cursor);
        });
    }

    fn resolve_shard_for_backfill(
        &self,
        logical_graph_name: &str,
        shard_id: u32,
    ) -> Result<ShardRegistryEntry, RouterError> {
        let entry = self.resolve_shard(shard_id)?;
        if entry.logical_graph_name != logical_graph_name {
            return Err(RouterError::InvalidArgument(format!(
                "shard {shard_id} is registered for graph {}, not {logical_graph_name}",
                entry.logical_graph_name
            )));
        }
        Ok(entry)
    }
}

fn build_backfill_args(
    cursor: &LabelBackfillShardState,
    max_vertices: u32,
) -> LabelPostingBackfillArgs {
    LabelPostingBackfillArgs {
        start_vertex_id: cursor.next_vertex_id,
        max_vertices,
    }
}

fn advance_backfill_cursor(
    cursor: &mut LabelBackfillShardState,
    result: &LabelPostingBackfillResult,
) {
    cursor.next_vertex_id = result.next_vertex_id;
    cursor.done = result.done;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::RouterInitArgs;
    use crate::types::AdminRegisterShardArgs;

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

    #[test]
    fn advance_backfill_cursor_updates_progress() {
        let mut cursor = LabelBackfillShardState::default();
        advance_backfill_cursor(
            &mut cursor,
            &LabelPostingBackfillResult {
                next_vertex_id: 42,
                vertices_processed: 10,
                postings_synced: 15,
                done: false,
            },
        );
        assert_eq!(cursor.next_vertex_id, 42);
        assert!(!cursor.done);

        advance_backfill_cursor(
            &mut cursor,
            &LabelPostingBackfillResult {
                next_vertex_id: 100,
                vertices_processed: 58,
                postings_synced: 0,
                done: true,
            },
        );
        assert_eq!(cursor.next_vertex_id, 100);
        assert!(cursor.done);
    }

    #[test]
    fn admin_label_backfill_step_advances_cursor() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let graph = graph_principal(1);
        let index = graph_principal(2);
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: 7,
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
                shard_id: 7,
                max_vertices: 32,
            },
            |_graph, args| async move {
                Ok(LabelPostingBackfillResult {
                    next_vertex_id: args.start_vertex_id.saturating_add(args.max_vertices),
                    vertices_processed: args.max_vertices,
                    postings_synced: 5,
                    done: false,
                })
            },
        ))
        .expect("step");

        assert_eq!(result.shard_id, 7);
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

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: 7,
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
                shard_id: 7,
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

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: 7,
                graph_canister: graph_principal(1),
                index_canister: graph_principal(2),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register shard");

        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|map| {
            map.insert(
                7,
                LabelBackfillShardState {
                    next_vertex_id: 99,
                    done: true,
                },
            );
        });

        let result = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: 7,
                max_vertices: 16,
            },
            |_graph, _args| async { unreachable!() },
        ))
        .expect("step");

        assert!(result.done);
        assert_eq!(result.next_vertex_id, 99);
        assert_eq!(result.vertices_processed, 0);
    }
}
