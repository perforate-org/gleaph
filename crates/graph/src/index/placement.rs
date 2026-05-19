//! Router placement protocol for federated graph shards.

use candid::Principal;
use gleaph_graph_kernel::federation::{
    CommitVertexPlacementArgs, LocalVertexId, LogicalVertexId, RouterError,
};
use ic_stable_lara::VertexId;
use std::cell::Cell;
use std::fmt;

#[derive(Clone, Debug)]
pub enum VertexPlacementError {
    Call(String),
    Rejected(RouterError),
}

impl fmt::Display for VertexPlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call(msg) => write!(f, "router placement call failed: {msg}"),
            Self::Rejected(err) => write!(f, "router rejected placement: {err:?}"),
        }
    }
}

impl std::error::Error for VertexPlacementError {}

thread_local! {
    static NATIVE_TEST_LOGICAL_COUNTER: Cell<u64> = const { Cell::new(0) };
    static NATIVE_TEST_PENDING_LOGICAL: Cell<Option<LogicalVertexId>> = const { Cell::new(None) };
}

pub fn allocate_logical_vertex_id(
    router_canister: Principal,
) -> Result<LogicalVertexId, VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        use ic_cdk::call::Call;

        let logical: Result<LogicalVertexId, RouterError> = Call::unbounded_wait(
            router_canister,
            "allocate_logical_vertex_id",
        )
        .wait()
        .map_err(|e| VertexPlacementError::Call(format!("{e:?}")))?
        .candid()
        .map_err(|e| VertexPlacementError::Call(format!("candid decode: {e}")))?;

        return logical.map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        let logical = NATIVE_TEST_LOGICAL_COUNTER.with(|c| {
            let next = c.get().saturating_add(1);
            c.set(next);
            next
        });
        NATIVE_TEST_PENDING_LOGICAL.with(|p| p.set(Some(logical)));
        Ok(logical)
    }
}

pub fn commit_vertex_placement(
    router_canister: Principal,
    args: CommitVertexPlacementArgs,
) -> Result<(), VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        use ic_cdk::call::Call;

        let (): Result<(), RouterError> = Call::unbounded_wait(router_canister, "commit_vertex_placement")
            .with_arg(&(args,))
            .wait()
            .map_err(|e| VertexPlacementError::Call(format!("{e:?}")))?
            .candid()
            .map_err(|e| VertexPlacementError::Call(format!("candid decode: {e}")))?;

        return ().map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        let pending = NATIVE_TEST_PENDING_LOGICAL
            .with(|p| p.take())
            .ok_or(VertexPlacementError::Rejected(
                RouterError::UnallocatedLogicalVertex,
            ))?;
        if pending != args.logical_vertex_id {
            return Err(VertexPlacementError::Rejected(
                RouterError::UnallocatedLogicalVertex,
            ));
        }
        let _ = args.local_vertex_id;
        Ok(())
    }
}

pub fn local_vertex_id_raw(vertex_id: VertexId) -> LocalVertexId {
    u32::from_le_bytes(vertex_id.to_le_bytes())
}
