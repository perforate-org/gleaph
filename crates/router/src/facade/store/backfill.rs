//! Posting backfill domain: router-stable cursors and shard orchestration.

use std::cell::RefCell;
use std::collections::BTreeSet;
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
    AdminLabelBackfillStepResult, AdminResetBackfillClaimArgs, AdminVertexPropertyBackfillStepArgs,
    AdminVertexPropertyBackfillStepResult, BackfillKind, EdgeBackfillShardStatus,
    LabelBackfillShardStatus, VertexPropertyBackfillShardStatus,
};

thread_local! {
    /// In-flight backfill steps, keyed by `(kind, shard)`. This is the concurrency
    /// guard for the read-await-write cursor cycle: a step claims its key before the
    /// inter-canister `await` so a concurrently routed step for the same shard is
    /// rejected instead of racing the cursor write-back.
    ///
    /// Heap-only on purpose. An upgrade wipes this set, and outstanding inter-canister
    /// calls do not resume across an upgrade, so any claim is implicitly released on
    /// upgrade — the wedge a persisted claim would cause cannot survive an upgrade.
    static INFLIGHT_BACKFILL: RefCell<BTreeSet<(BackfillKind, GraphShardKey)>> =
        const { RefCell::new(BTreeSet::new()) };
}

/// Claims `(kind, key)` for the duration of a step. Returns `false` (and changes
/// nothing) if the key is already claimed. The matching release is
/// [`release_inflight_backfill`]; on the normal Ok/Err paths the step releases it,
/// and an upgrade clears the whole set.
fn claim_inflight_backfill(kind: BackfillKind, key: GraphShardKey) -> bool {
    INFLIGHT_BACKFILL.with_borrow_mut(|set| set.insert((kind, key)))
}

fn release_inflight_backfill(kind: BackfillKind, key: GraphShardKey) {
    INFLIGHT_BACKFILL.with_borrow_mut(|set| {
        set.remove(&(kind, key));
    });
}

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
        // Claim the shard before the inter-canister `await`. The IC may interleave
        // another ingress step for this shard at the await boundary; the heap claim
        // makes that concurrent step bail here instead of reading a stale cursor and
        // racing the write-back.
        if !claim_inflight_backfill(BackfillKind::Label, cursor_key) {
            return Err(RouterError::Conflict(
                "label backfill step already in progress for this shard; retry after it returns"
                    .into(),
            ));
        }

        let result = match call_backfill(
            shard.graph_canister,
            PostingBackfillArgs {
                start_vertex_id: cursor.next_vertex_id,
                max_vertices: args.max_vertices,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                // Release the claim so a transient remote failure stays retryable.
                release_inflight_backfill(BackfillKind::Label, cursor_key);
                return Err(RouterError::Internal(err));
            }
        };
        cursor.apply_batch_progress(result.next_vertex_id, result.done);
        self.store_label_backfill_state(cursor_key, cursor);
        release_inflight_backfill(BackfillKind::Label, cursor_key);

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
        // Claim the shard before the inter-canister `await` (see label step).
        if !claim_inflight_backfill(BackfillKind::VertexProperty, cursor_key) {
            return Err(RouterError::Conflict(
                "vertex-property backfill step already in progress for this shard; retry after it returns"
                    .into(),
            ));
        }

        let result = match call_backfill(
            shard.graph_canister,
            PostingBackfillArgs {
                start_vertex_id: cursor.next_vertex_id,
                max_vertices: args.max_vertices,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                release_inflight_backfill(BackfillKind::VertexProperty, cursor_key);
                return Err(RouterError::Internal(err));
            }
        };
        cursor.apply_batch_progress(result.next_vertex_id, result.done);
        self.store_vertex_property_backfill_state(cursor_key, cursor);
        release_inflight_backfill(BackfillKind::VertexProperty, cursor_key);

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
        // Claim the shard before the inter-canister `await` (see label step).
        if !claim_inflight_backfill(BackfillKind::Edge, cursor_key) {
            return Err(RouterError::Conflict(
                "edge backfill step already in progress for this shard; retry after it returns"
                    .into(),
            ));
        }

        let result = match call_backfill(
            shard.graph_canister,
            EdgePostingBackfillArgs {
                after_key: cursor.after_key.clone(),
                max_entries: args.max_entries,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                release_inflight_backfill(BackfillKind::Edge, cursor_key);
                return Err(RouterError::Internal(err));
            }
        };
        cursor.apply_batch_progress(result.next_after_key.clone(), result.done);
        self.store_edge_backfill_state(cursor_key, cursor);
        release_inflight_backfill(BackfillKind::Edge, cursor_key);

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

    /// Operator recovery for a wedged backfill claim: releases the in-flight claim
    /// on one shard for the given kind. The heap claim normally clears on the step's
    /// Ok/Err paths and on upgrade; this is the manual escape hatch for the residual
    /// case where the router's own reply callback trapped after the call returned.
    /// Releasing an unheld claim is a no-op, and the cursor position is untouched.
    pub(crate) fn admin_reset_backfill_claim(
        &self,
        caller: Principal,
        args: &AdminResetBackfillClaimArgs,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        let shard = self.resolve_shard_for_backfill(&args.logical_graph_name, args.shard_id)?;
        let cursor_key = GraphShardKey::new(shard.graph_id, args.shard_id);
        release_inflight_backfill(args.kind, cursor_key);
        Ok(())
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

/// Deletes the three posting-backfill cursors for one shard. Backfill cursors are
/// derived per-shard state, so they are owned by the shard lifecycle: dropping them
/// on `unregister_shard` prevents orphaned cursors and stops a later shard reusing
/// the same `(graph_id, shard_id)` from inheriting a stale cursor (which could skip
/// its historical backfill or wedge on a leftover in-progress claim).
pub(super) fn purge_backfill_state(key: GraphShardKey) {
    ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|map| {
        map.remove(&key);
    });
    ROUTER_VERTEX_PROPERTY_BACKFILL_STATE.with_borrow_mut(|map| {
        map.remove(&key);
    });
    ROUTER_EDGE_BACKFILL_STATE.with_borrow_mut(|map| {
        map.remove(&key);
    });
    // Drop any heap claim too, so re-registering the shard does not inherit a stale
    // in-flight guard for the (now removed) cursors.
    INFLIGHT_BACKFILL.with_borrow_mut(|set| {
        set.remove(&(BackfillKind::Label, key));
        set.remove(&(BackfillKind::VertexProperty, key));
        set.remove(&(BackfillKind::Edge, key));
    });
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
            provision_canister: None,
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
        let admin = Principal::from_slice(&[1; 29]);
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
        let admin = Principal::from_slice(&[1; 29]);
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
        let admin = Principal::from_slice(&[1; 29]);
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
        let admin = Principal::from_slice(&[1; 29]);
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
        let admin = Principal::from_slice(&[1; 29]);
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

    fn setup_one_shard() -> (RouterStore, Principal, GraphShardKey) {
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
        let graph_id = lookup_graph_id("tenant.main").expect("graph id");
        (store, admin, GraphShardKey::new(graph_id, ShardId::new(0)))
    }

    #[test]
    fn label_step_claims_cursor_before_await_and_clears_after_success() {
        let (store, admin, key) = setup_one_shard();

        let result = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 32,
            },
            |_graph, args| async move {
                // The claim must be held before this await body runs, so a
                // concurrently routed step would observe the claim and bail.
                assert!(
                    INFLIGHT_BACKFILL.with_borrow(|s| s.contains(&(BackfillKind::Label, key))),
                    "shard must be claimed before the await"
                );
                Ok(PostingBackfillResult {
                    next_vertex_id: args.start_vertex_id.saturating_add(args.max_vertices),
                    vertices_processed: args.max_vertices,
                    postings_synced: 5,
                    done: false,
                })
            },
        ))
        .expect("step");
        assert!(!result.done);

        assert!(
            !INFLIGHT_BACKFILL.with_borrow(|s| s.contains(&(BackfillKind::Label, key))),
            "claim cleared after success"
        );
        let after = ROUTER_LABEL_BACKFILL_STATE.with_borrow(|m| m.get(&key).unwrap_or_default());
        assert_eq!(after.next_vertex_id, 32);
    }

    #[test]
    fn concurrent_label_step_is_rejected_while_in_progress() {
        let (store, admin, key) = setup_one_shard();
        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|m| {
            m.insert(
                key,
                BackfillShardState {
                    next_vertex_id: 10,
                    done: false,
                },
            );
        });
        // Simulate an in-flight sibling step holding the claim.
        assert!(claim_inflight_backfill(BackfillKind::Label, key));

        let err = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 8,
            },
            |_graph, _args| async { unreachable!("must not call remote while claimed") },
        ))
        .expect_err("claimed shard must reject a concurrent step");
        assert!(matches!(err, RouterError::Conflict(_)));

        let after = ROUTER_LABEL_BACKFILL_STATE.with_borrow(|m| m.get(&key).unwrap_or_default());
        assert_eq!(
            after.next_vertex_id, 10,
            "rejected step must not move cursor"
        );
        assert!(
            INFLIGHT_BACKFILL.with_borrow(|s| s.contains(&(BackfillKind::Label, key))),
            "the in-flight claim is untouched"
        );
    }

    #[test]
    fn failed_label_step_releases_claim_without_advancing() {
        let (store, admin, key) = setup_one_shard();

        let err = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 8,
            },
            |_graph, _args| async { Err("remote exploded".to_string()) },
        ))
        .expect_err("remote failure surfaces");
        assert!(matches!(err, RouterError::Internal(_)));

        assert!(
            !INFLIGHT_BACKFILL.with_borrow(|s| s.contains(&(BackfillKind::Label, key))),
            "claim released so the step stays retryable"
        );
        let after = ROUTER_LABEL_BACKFILL_STATE.with_borrow(|m| m.get(&key).unwrap_or_default());
        assert_eq!(
            after.next_vertex_id, 0,
            "failed step must not advance cursor"
        );
    }

    #[test]
    fn edge_step_claims_and_clears_in_progress() {
        let (store, admin, key) = setup_one_shard();
        let returned_key = vec![7u8; gleaph_graph_kernel::federation::EDGE_PROPERTY_KEY_BYTES];
        let assert_key = returned_key.clone();

        let result = futures::executor::block_on(store.admin_edge_backfill_step(
            admin,
            AdminEdgeBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_entries: 16,
            },
            move |_graph, _args| {
                let returned_key = returned_key.clone();
                async move {
                    assert!(
                        INFLIGHT_BACKFILL.with_borrow(|s| s.contains(&(BackfillKind::Edge, key))),
                        "edge shard must be claimed before the await"
                    );
                    Ok(EdgePostingBackfillResult {
                        next_after_key: Some(returned_key),
                        entries_processed: 16,
                        postings_synced: 2,
                        done: false,
                    })
                }
            },
        ))
        .expect("edge step");
        assert!(!result.done);

        assert!(
            !INFLIGHT_BACKFILL.with_borrow(|s| s.contains(&(BackfillKind::Edge, key))),
            "edge claim cleared after success"
        );
        let after = ROUTER_EDGE_BACKFILL_STATE.with_borrow(|m| m.get(&key).unwrap_or_default());
        assert_eq!(after.after_key, Some(assert_key));
    }

    #[test]
    fn unregister_shard_purges_backfill_cursors() {
        let (store, admin, key) = setup_one_shard();
        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|m| {
            m.insert(
                key,
                BackfillShardState {
                    next_vertex_id: 7,
                    done: true,
                },
            );
        });
        ROUTER_VERTEX_PROPERTY_BACKFILL_STATE.with_borrow_mut(|m| {
            m.insert(
                key,
                BackfillShardState {
                    next_vertex_id: 9,
                    done: false,
                },
            );
        });
        ROUTER_EDGE_BACKFILL_STATE.with_borrow_mut(|m| {
            m.insert(
                key,
                EdgeBackfillShardState {
                    after_key: Some(vec![
                        1u8;
                        gleaph_graph_kernel::federation::EDGE_PROPERTY_KEY_BYTES
                    ]),
                    done: true,
                },
            );
        });

        futures::executor::block_on(store.admin_unregister_shard(
            admin,
            "tenant.main",
            ShardId::new(0),
        ))
        .expect("unregister");

        assert!(
            ROUTER_LABEL_BACKFILL_STATE.with_borrow(|m| m.get(&key).is_none()),
            "label cursor purged on unregister"
        );
        assert!(
            ROUTER_VERTEX_PROPERTY_BACKFILL_STATE.with_borrow(|m| m.get(&key).is_none()),
            "vertex-property cursor purged on unregister"
        );
        assert!(
            ROUTER_EDGE_BACKFILL_STATE.with_borrow(|m| m.get(&key).is_none()),
            "edge cursor purged on unregister"
        );
    }

    #[test]
    fn reset_backfill_claim_clears_in_progress_without_moving_cursor() {
        let (store, admin, key) = setup_one_shard();
        ROUTER_LABEL_BACKFILL_STATE.with_borrow_mut(|m| {
            m.insert(
                key,
                BackfillShardState {
                    next_vertex_id: 42,
                    done: false,
                },
            );
        });
        assert!(claim_inflight_backfill(BackfillKind::Label, key));

        store
            .admin_reset_backfill_claim(
                admin,
                &AdminResetBackfillClaimArgs {
                    logical_graph_name: "tenant.main".into(),
                    shard_id: ShardId::new(0),
                    kind: BackfillKind::Label,
                },
            )
            .expect("reset");

        assert!(
            !INFLIGHT_BACKFILL.with_borrow(|s| s.contains(&(BackfillKind::Label, key))),
            "claim cleared"
        );
        let after = ROUTER_LABEL_BACKFILL_STATE.with_borrow(|m| m.get(&key).unwrap_or_default());
        assert_eq!(after.next_vertex_id, 42, "reset must not move the cursor");
        assert!(!after.done);
    }

    #[test]
    fn reset_backfill_claim_requires_admin() {
        let (store, _admin, _key) = setup_one_shard();
        let intruder = Principal::from_slice(&[9; 29]);
        let err = store
            .admin_reset_backfill_claim(
                intruder,
                &AdminResetBackfillClaimArgs {
                    logical_graph_name: "tenant.main".into(),
                    shard_id: ShardId::new(0),
                    kind: BackfillKind::Edge,
                },
            )
            .expect_err("non-admin rejected");
        assert!(matches!(err, RouterError::NotAuthorized));
    }

    #[test]
    fn upgrade_releases_inflight_claims_so_steps_resume() {
        let (store, admin, key) = setup_one_shard();
        // A reply callback trapped after the call returned, leaving the claim held.
        assert!(claim_inflight_backfill(BackfillKind::Label, key));

        // An upgrade wipes heap state; model that by clearing the set. Outstanding
        // inter-canister calls do not resume across an upgrade, so this cannot drop a
        // genuinely live claim.
        INFLIGHT_BACKFILL.with_borrow_mut(|s| s.clear());

        let result = futures::executor::block_on(store.admin_label_backfill_step(
            admin,
            AdminLabelBackfillStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_vertices: 8,
            },
            |_graph, args| async move {
                Ok(PostingBackfillResult {
                    next_vertex_id: args.start_vertex_id.saturating_add(args.max_vertices),
                    vertices_processed: args.max_vertices,
                    postings_synced: 0,
                    done: true,
                })
            },
        ))
        .expect("step proceeds after upgrade cleared the stale claim");
        assert!(result.done);
    }
}
