//! Graph-index-private durable state for one physical index build.

use std::borrow::Cow;

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::entry::{GraphId, IndexNameId};
use gleaph_graph_kernel::index::{
    IndexBuildPhase, IndexBuildProgress, IndexBuildSealStatus, IndexBuildShardWatermark,
    IndexBuildStatus, IndexBuildTarget, PhysicalIndexId, RegisterIndexBuildRequest,
};
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use serde::Deserialize;

/// Immutable registration stored under the map's `PhysicalIndexId` key.
///
/// The physical namespace is deliberately absent here so the build generation has one stable
/// representation instead of being duplicated in both map key and value.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize)]
pub(crate) struct IndexBuildScopeRecord {
    pub(crate) graph_id: GraphId,
    pub(crate) index_name_id: IndexNameId,
    pub(crate) catalog_epoch: u64,
    pub(crate) topology_epoch: u64,
    pub(crate) target: IndexBuildTarget,
}

impl From<&RegisterIndexBuildRequest> for IndexBuildScopeRecord {
    fn from(request: &RegisterIndexBuildRequest) -> Self {
        Self {
            graph_id: request.graph_id,
            index_name_id: request.index_name_id,
            catalog_epoch: request.catalog_epoch,
            topology_epoch: request.topology_epoch,
            target: request.target.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize)]
pub(crate) struct IndexBuildLastPage {
    pub(crate) sequence: u64,
    pub(crate) fingerprint: [u8; 32],
    pub(crate) inserted_facts: u32,
    pub(crate) skipped_touched_facts: u32,
}

/// O(1) contiguous DML acknowledgement and exact replay receipt for one target shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize)]
pub(crate) struct IndexBuildShardState {
    pub(crate) shard_id: u32,
    pub(crate) acknowledged_through: u64,
    pub(crate) last_fingerprint: Option<[u8; 32]>,
    pub(crate) seal_target: Option<u64>,
}

impl IndexBuildShardState {
    fn watermark(&self) -> IndexBuildShardWatermark {
        IndexBuildShardWatermark {
            shard_id: self.shard_id,
            admitted_through: self.seal_target.unwrap_or(self.acknowledged_through),
            drained_through: self.acknowledged_through,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Deserialize)]
pub(crate) enum IndexBuildCleanupPhase {
    VertexPostings,
    EdgePostings,
    TouchedSubjects,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize)]
pub(crate) struct IndexBuildCleanupCursor {
    pub(crate) phase: IndexBuildCleanupPhase,
    pub(crate) resume_key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize)]
pub(crate) enum IndexBuildLifecycle {
    Building,
    Sealing { seal_catalog_epoch: u64 },
    Aborting { cleanup: IndexBuildCleanupCursor },
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize)]
pub(crate) struct IndexBuildState {
    pub(crate) scope: IndexBuildScopeRecord,
    pub(crate) next_page_sequence: u64,
    pub(crate) next_shard_index: u32,
    pub(crate) cursor: Option<Vec<u8>>,
    pub(crate) seeded_items: u64,
    pub(crate) last_page: Option<IndexBuildLastPage>,
    pub(crate) shards: Vec<IndexBuildShardState>,
    pub(crate) lifecycle: IndexBuildLifecycle,
}

impl IndexBuildState {
    pub(crate) fn new(request: &RegisterIndexBuildRequest) -> Self {
        Self {
            scope: request.into(),
            next_page_sequence: 0,
            next_shard_index: 0,
            cursor: None,
            seeded_items: 0,
            last_page: None,
            shards: request
                .target_shard_ids
                .iter()
                .copied()
                .map(|shard_id| IndexBuildShardState {
                    shard_id,
                    acknowledged_through: 0,
                    last_fingerprint: None,
                    seal_target: None,
                })
                .collect(),
            lifecycle: IndexBuildLifecycle::Building,
        }
    }

    pub(crate) fn registration(
        &self,
        physical_index_id: PhysicalIndexId,
    ) -> RegisterIndexBuildRequest {
        RegisterIndexBuildRequest {
            physical_index_id,
            graph_id: self.scope.graph_id,
            index_name_id: self.scope.index_name_id,
            catalog_epoch: self.scope.catalog_epoch,
            topology_epoch: self.scope.topology_epoch,
            target: self.scope.target.clone(),
            target_shard_ids: self.shards.iter().map(|shard| shard.shard_id).collect(),
        }
    }

    #[inline]
    pub(crate) fn done(&self) -> bool {
        usize::try_from(self.next_shard_index)
            .map(|index| index == self.shards.len())
            .unwrap_or(false)
    }

    pub(crate) fn progress(&self) -> IndexBuildProgress {
        let done = self.done();
        let expected_shard_id = if done {
            None
        } else {
            usize::try_from(self.next_shard_index)
                .ok()
                .and_then(|index| self.shards.get(index).map(|shard| shard.shard_id))
        };
        IndexBuildProgress {
            next_page_sequence: self.next_page_sequence,
            next_shard_index: self.next_shard_index,
            expected_shard_id,
            cursor: self.cursor.clone(),
            seeded_items: self.seeded_items,
            done,
        }
    }

    pub(crate) fn status(&self, physical_index_id: PhysicalIndexId) -> IndexBuildStatus {
        IndexBuildStatus {
            registration: self.registration(physical_index_id),
            progress: self.progress(),
            phase: match &self.lifecycle {
                IndexBuildLifecycle::Building => IndexBuildPhase::Building,
                IndexBuildLifecycle::Sealing { seal_catalog_epoch } => IndexBuildPhase::Sealing {
                    seal_catalog_epoch: *seal_catalog_epoch,
                },
                IndexBuildLifecycle::Aborting { .. } => IndexBuildPhase::Aborting,
                IndexBuildLifecycle::Aborted => IndexBuildPhase::Aborted,
            },
            watermarks: self.watermarks(),
        }
    }

    pub(crate) fn watermarks(&self) -> Vec<IndexBuildShardWatermark> {
        self.shards
            .iter()
            .map(IndexBuildShardState::watermark)
            .collect()
    }

    pub(crate) fn seal_status(&self) -> Option<IndexBuildSealStatus> {
        let IndexBuildLifecycle::Sealing { seal_catalog_epoch } = &self.lifecycle else {
            return None;
        };
        Some(IndexBuildSealStatus {
            base_complete: self.done(),
            seal_catalog_epoch: *seal_catalog_epoch,
            watermarks: self.watermarks(),
        })
    }
}

impl Storable for IndexBuildState {
    // ADR 0059 is a breaking pre-release layout. Decode exactly this shape; no legacy branch.
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("IndexBuildState must encode"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("IndexBuildState must encode")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("invalid ADR 0059 IndexBuildState encoding")
    }
}
