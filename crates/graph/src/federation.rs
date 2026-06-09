//! Federation execution boundary for graph query runtime.
//!
//! Standalone mode binds local index hits only. Target multi-shard behavior is documented
//! under `design/sharding/federation-target.md`.

mod index_bind;
mod routing;

pub(crate) use index_bind::{bind_local_index_hits, materialize_federated_index_hits};
pub(crate) use routing::federation_routing;

use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::PostingHit;

use crate::facade::GraphStore;
use crate::plan::query::PlanRow;

/// Query-time federation policy (standalone implementation today).
pub trait FederationPort {
    #[expect(
        dead_code,
        reason = "used by federated implementations and router seeds"
    )]
    fn local_shard_id(&self) -> ShardId;

    /// Bind index hits onto plan rows for `variable`.
    fn bind_index_hits(
        &self,
        store: &GraphStore,
        rows: &[PlanRow],
        variable: &str,
        hits: &[PostingHit],
    ) -> Vec<PlanRow>;
}

/// Single-shard mode: keep hits for [`Self::local_shard_id`] and bind [`PlanBinding::Vertex`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StandaloneFederation {
    local_shard_id: ShardId,
}

impl StandaloneFederation {
    pub fn from_store(store: &GraphStore) -> Self {
        Self {
            local_shard_id: store
                .federation_routing()
                .map(|routing| routing.shard_id)
                .unwrap_or(0),
        }
    }

    #[inline]
    #[expect(dead_code, reason = "tests and future router dispatch")]
    pub fn new(local_shard_id: ShardId) -> Self {
        Self { local_shard_id }
    }
}

impl FederationPort for StandaloneFederation {
    fn local_shard_id(&self) -> ShardId {
        self.local_shard_id
    }

    fn bind_index_hits(
        &self,
        store: &GraphStore,
        rows: &[PlanRow],
        variable: &str,
        hits: &[PostingHit],
    ) -> Vec<PlanRow> {
        bind_local_index_hits(store, rows, variable, hits, self.local_shard_id)
    }
}
