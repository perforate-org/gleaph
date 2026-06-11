//! Federation execution boundary for graph query runtime.
//!
//! Index bind and peer expand entry points for the executor. Target multi-shard behavior is
//! documented under `design/sharding/federation-target.md`.

mod expand;
mod index_bind;
mod routing;

pub(crate) use expand::{
    TraversalExpandSource, federated_direction_for_expand, federated_expand_label_id_raw,
    resolve_traversal_expand_local_csr, resolve_traversal_expand_source,
};
pub(crate) use index_bind::bind_local_index_hits;
pub(crate) use routing::federation_routing;

use gleaph_graph_kernel::federation::{FederatedExpandArgs, FederatedExpandNeighbor, ShardId};
use gleaph_graph_kernel::index::PostingHit;

use crate::facade::GraphStore;
use crate::plan::query::PlanQueryError;
use crate::plan::query::PlanRow;

/// Query-time federation policy (standalone implementation today).
///
/// Index binding is synchronous; cross-shard traverse uses [`StandaloneFederation::peer_expand`].
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
                .unwrap_or(ShardId::new(0)),
        }
    }

    #[inline]
    #[expect(dead_code, reason = "tests and future router dispatch")]
    pub fn new(local_shard_id: ShardId) -> Self {
        Self { local_shard_id }
    }

    /// Cross-shard neighbor lookup when expand cannot use local CSR.
    pub async fn peer_expand(
        &self,
        store: &GraphStore,
        args: FederatedExpandArgs,
    ) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
        expand::peer_expand(store, args).await
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
