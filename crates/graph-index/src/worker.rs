//! Graph-index-owned bounded canonical pull worker for ADR 0059.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::canonical_export::{
    CanonicalExportError, CanonicalExportPage, CanonicalExportRequest,
};
use gleaph_graph_kernel::index::{
    IndexBuildControlRequest, IndexBuildError, IndexBuildStatus, MAX_INDEX_BUILD_ADVANCE_PAGES,
};

use crate::facade::IndexStore;

#[cfg(target_family = "wasm")]
const ADVANCE_INSTRUCTION_RESERVE: u64 = 1_000_000_000;
#[cfg(target_family = "wasm")]
const UPDATE_INSTRUCTION_LIMIT: u64 = 40_000_000_000;

#[inline]
fn near_instruction_limit() -> bool {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::instruction_counter()
            >= UPDATE_INSTRUCTION_LIMIT.saturating_sub(ADVANCE_INSTRUCTION_RESERVE)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        false
    }
}

/// Pulls and atomically applies up to the fixed per-call page limit.
///
/// `fetch` returns an outer transport outcome and the Graph's compact typed result. No stable
/// progress is mutated before a successful response has been fully decoded and validated.
pub(crate) async fn advance_index_build_with<F, Fut>(
    caller: Principal,
    control: IndexBuildControlRequest,
    mut fetch: F,
) -> Result<IndexBuildStatus, IndexBuildError>
where
    F: FnMut(Principal, CanonicalExportRequest) -> Fut,
    Fut: Future<Output = Result<Result<CanonicalExportPage, CanonicalExportError>, ()>>,
{
    let store = IndexStore::new();
    for _ in 0..MAX_INDEX_BUILD_ADVANCE_PAGES {
        if near_instruction_limit() {
            break;
        }
        let Some(prepared) = store.prepare_index_build_pull(caller, &control)? else {
            break;
        };
        let page = fetch(prepared.graph_canister, prepared.export.clone())
            .await
            .map_err(|()| IndexBuildError::Transport)?
            .map_err(IndexBuildError::Graph)?;
        store
            .apply_index_build_pull(caller, &control, &prepared, page)
            .map_err(IndexBuildError::from)?;
    }
    store
        .index_build_status(caller, control.registration.physical_index_id)
        .map_err(IndexBuildError::from)
}

#[cfg(target_family = "wasm")]
async fn fetch_index_export_page(
    graph_canister: Principal,
    request: CanonicalExportRequest,
) -> Result<Result<CanonicalExportPage, CanonicalExportError>, ()> {
    use ic_cdk::call::Call;

    Call::bounded_wait(graph_canister, "index_export_page")
        .with_arg(&request)
        .await
        .map_err(|_| ())?
        .candid()
        .map_err(|_| ())
}

#[cfg(not(target_family = "wasm"))]
async fn fetch_index_export_page(
    _graph_canister: Principal,
    _request: CanonicalExportRequest,
) -> Result<Result<CanonicalExportPage, CanonicalExportError>, ()> {
    Err(())
}

pub(crate) async fn advance_index_build(
    caller: Principal,
    control: IndexBuildControlRequest,
) -> Result<IndexBuildStatus, IndexBuildError> {
    advance_index_build_with(caller, control, fetch_index_export_page).await
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::ready;

    use gleaph_graph_kernel::canonical_export::{
        CanonicalExportPage, CanonicalExportTarget, CanonicalIndexableFact,
    };
    use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        IndexBuildControlRequest, IndexBuildTarget, PhysicalIndexId, RegisterIndexBuildRequest,
    };

    use super::*;
    use crate::init::IndexInitArgs;

    const GRAPH_ID: GraphId = GraphId::from_raw(1);
    const PROPERTY_ID: PropertyId = PropertyId::from_raw(42);

    fn setup() -> (IndexStore, Principal, Principal, Principal) {
        let store = IndexStore::new();
        let router = Principal::from_slice(&[61]);
        let shard0 = Principal::from_slice(&[62]);
        let shard1 = Principal::from_slice(&[63]);
        store
            .init_from_args(&IndexInitArgs {
                router_canister: router,
            })
            .expect("initialize graph-index");
        for (shard_id, principal) in [(ShardId::new(0), shard0), (ShardId::new(1), shard1)] {
            store
                .admin_attach_shard_canister(router, GRAPH_ID, 2, 0, shard_id, principal)
                .expect("attach Graph shard");
        }
        (store, router, shard0, shard1)
    }

    fn control(raw: u64) -> IndexBuildControlRequest {
        IndexBuildControlRequest {
            registration: RegisterIndexBuildRequest {
                physical_index_id: PhysicalIndexId::new(raw).expect("physical id"),
                graph_id: GRAPH_ID,
                index_name_id: IndexNameId::from_raw(7),
                catalog_epoch: 11,
                topology_epoch: 17,
                target: IndexBuildTarget::Vertex {
                    label_id: 8,
                    property_id: PROPERTY_ID,
                },
                target_shard_ids: vec![0, 1],
            },
        }
    }

    fn vertex(vertex_id: u32, value: &[u8]) -> CanonicalIndexableFact {
        CanonicalIndexableFact::Vertex {
            vertex_id,
            property_id: PROPERTY_ID,
            encoded_value: value.to_vec(),
        }
    }

    #[test]
    fn worker_pulls_multiple_shards_and_sparse_empty_pages() {
        let (store, router, shard0, shard1) = setup();
        let control = control(2_001);
        store
            .register_index_build(router, &control.registration)
            .expect("register build");
        let mut replies = VecDeque::from([
            Ok(Ok(CanonicalExportPage {
                facts: vec![vertex(1, b"a")],
                next: Some(vec![1]),
                done: false,
            })),
            Ok(Ok(CanonicalExportPage {
                facts: Vec::new(),
                next: None,
                done: true,
            })),
            Ok(Ok(CanonicalExportPage {
                facts: vec![vertex(2, b"b")],
                next: None,
                done: true,
            })),
        ]);
        let mut calls = Vec::new();
        let status = futures::executor::block_on(advance_index_build_with(
            router,
            control.clone(),
            |principal, request| {
                calls.push((principal, request));
                ready(replies.pop_front().expect("bounded fake reply"))
            },
        ))
        .expect("advance build");

        assert!(status.progress.done);
        assert_eq!(status.progress.next_page_sequence, 3);
        assert_eq!(status.progress.seeded_items, 2);
        assert_eq!(calls.len(), 3);
        assert_eq!(
            [calls[0].0, calls[1].0, calls[2].0],
            [shard0, shard0, shard1]
        );
        assert_eq!(calls[0].1.cursor, None);
        assert_eq!(calls[1].1.cursor, Some(vec![1]));
        assert_eq!(calls[2].1.cursor, None);
        for (_, request) in calls {
            assert_eq!(request.graph_id, GRAPH_ID);
            assert_eq!(request.index_name_id, IndexNameId::from_raw(7));
            assert_eq!(
                request.physical_index_id,
                control.registration.physical_index_id
            );
            assert_eq!(request.catalog_epoch, 11);
            assert_eq!(
                request.target,
                CanonicalExportTarget::Vertex {
                    label_id: 8,
                    property_id: PROPERTY_ID
                }
            );
        }
    }

    #[test]
    fn lost_reply_leaves_cursor_unchanged_and_exact_retry_resumes() {
        let (store, router, _, _) = setup();
        let control = control(2_002);
        store
            .register_index_build(router, &control.registration)
            .expect("register build");
        let error = futures::executor::block_on(advance_index_build_with(
            router,
            control.clone(),
            |_principal, _request| ready(Err(())),
        ))
        .expect_err("transport loss remains retryable");
        assert_eq!(error, IndexBuildError::Transport);
        assert!(error.is_retryable());
        let unchanged = store
            .index_build_status(router, control.registration.physical_index_id)
            .expect("status after lost reply");
        assert_eq!(unchanged.progress.next_page_sequence, 0);
        assert_eq!(unchanged.progress.cursor, None);

        let mut replies = VecDeque::from([
            Ok(Ok(CanonicalExportPage {
                facts: vec![vertex(3, b"retry")],
                next: None,
                done: true,
            })),
            Ok(Ok(CanonicalExportPage {
                facts: Vec::new(),
                next: None,
                done: true,
            })),
        ]);
        let status = futures::executor::block_on(advance_index_build_with(
            router,
            control,
            |_principal, _request| ready(replies.pop_front().expect("retry page")),
        ))
        .expect("exact retry resumes");
        assert!(status.progress.done);
        assert_eq!(status.progress.next_page_sequence, 2);
    }

    #[test]
    fn concurrent_duplicate_page_callbacks_converge_as_exact_replay() {
        let (store, router, _, _) = setup();
        let control = control(2_004);
        store
            .register_index_build(router, &control.registration)
            .expect("register build");
        let prepared = store
            .prepare_index_build_pull(router, &control)
            .expect("prepare first pull")
            .expect("build has work");
        let duplicate = prepared.clone();
        let page = CanonicalExportPage {
            facts: vec![vertex(4, b"concurrent")],
            next: Some(vec![1]),
            done: false,
        };

        let first = store
            .apply_index_build_pull(router, &control, &prepared, page.clone())
            .expect("first callback applies");
        let replay = store
            .apply_index_build_pull(router, &control, &duplicate, page)
            .expect("duplicate callback replays exactly");

        assert_eq!(first, replay);
        assert_eq!(replay.progress.next_page_sequence, 1);
        assert_eq!(replay.progress.seeded_items, 1);
    }

    #[test]
    fn control_scope_mismatch_rejects_before_graph_call() {
        let (store, router, _, _) = setup();
        let mut wrong = control(2_003);
        store
            .register_index_build(router, &wrong.registration)
            .expect("register build");
        wrong.registration.topology_epoch += 1;
        let mut called = false;
        let error = futures::executor::block_on(advance_index_build_with(
            router,
            wrong,
            |_principal, _request| {
                called = true;
                ready(Err(()))
            },
        ))
        .expect_err("mismatched control scope");
        assert_eq!(
            error,
            IndexBuildError::Store(
                gleaph_graph_kernel::index::IndexBuildStoreError::InvalidControl
            )
        );
        assert!(!called);
    }
}
