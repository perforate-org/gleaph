//! Incremental vertex migration types (journal, queue, per-shard state).

use candid::{CandidType, Decode, Encode};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use super::{LocalVertexId, LogicalVertexId, PhysicalVertexLocation, ShardId};
use crate::entry::{EdgeLabelId, PropertyId, VertexLabelId};

/// Per-shard physical migration state for one local vertex row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum VertexMigrationState {
    /// Normal authoritative or non-migrating row.
    Active,
    /// Source shard: authoritative, writes journaled.
    SourceMigrating { epoch: u64 },
    /// Destination shard: invisible to queries, maintenance-only writes.
    TargetStaging { epoch: u64 },
    /// Source shard after cutover: resolves via router / cached destination.
    ForwardingStub {
        logical_vertex_id: LogicalVertexId,
        cached_location: PhysicalVertexLocation,
        epoch: u64,
    },
}

/// Resume phase for chunked copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MigrationPhase {
    VertexMetadata,
    OutEdges,
    InReverse,
    JournalDrain,
    Finalize,
    Done,
}

/// CSR orientation being copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MigrationOrientation {
    Out,
    InReverse,
}

/// Stable cursor: label bucket + physical slot (not logical offset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EdgeCopyCursor {
    pub label_raw: u32,
    pub slot_index: u32,
}

/// Persistent migration work item (resumable after interruption).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MigrationItem {
    pub logical_vertex_id: LogicalVertexId,
    pub epoch: u64,
    pub source_shard_id: ShardId,
    pub source_local_vertex_id: LocalVertexId,
    pub target_shard_id: ShardId,
    pub target_local_vertex_id: LocalVertexId,
    pub phase: MigrationPhase,
    pub orientation: MigrationOrientation,
    pub label_cursor: EdgeCopyCursor,
    pub edge_cursor: EdgeCopyCursor,
    pub copy_start_seq: u64,
    pub copied_until_seq: u64,
    pub drained_until_seq: u64,
    pub final_seq: Option<u64>,
    pub bulk_limit: u32,
}

impl MigrationItem {
    pub const INITIAL_BULK_LIMIT: u32 = 64;
    pub const MIN_BULK_LIMIT: u32 = 1;
    pub const MAX_BULK_LIMIT: u32 = 4096;

    pub fn new(
        logical_vertex_id: LogicalVertexId,
        epoch: u64,
        source_shard_id: ShardId,
        source_local_vertex_id: LocalVertexId,
        target_shard_id: ShardId,
    ) -> Self {
        Self {
            logical_vertex_id,
            epoch,
            source_shard_id,
            source_local_vertex_id,
            target_shard_id,
            target_local_vertex_id: 0,
            phase: MigrationPhase::VertexMetadata,
            orientation: MigrationOrientation::Out,
            label_cursor: EdgeCopyCursor::default(),
            edge_cursor: EdgeCopyCursor::default(),
            copy_start_seq: 0,
            copied_until_seq: 0,
            drained_until_seq: 0,
            final_seq: None,
            bulk_limit: Self::INITIAL_BULK_LIMIT,
        }
    }
}

/// Wire handle for migration handle maps (shard-local dense ids + label bucket raw).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, CandidType, Serialize, Deserialize)]
pub struct MigrationEdgeHandleWire {
    pub owner_local_vertex_id: LocalVertexId,
    pub label_raw: u32,
    pub slot_index: u32,
}

/// Journal operation during source migration.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MigrationJournalOp {
    VertexLabelAdded {
        label_id: VertexLabelId,
    },
    VertexLabelRemoved {
        label_id: VertexLabelId,
    },
    VertexPropertySet {
        property_id: PropertyId,
        payload_bytes: Vec<u8>,
    },
    VertexPropertyRemoved {
        property_id: PropertyId,
    },
    OutEdgeAdded {
        catalog_label: Option<EdgeLabelId>,
        undirected: bool,
        payload_bytes: Vec<u8>,
        target_logical_vertex_id: LogicalVertexId,
        target_is_remote: bool,
        source_handle: MigrationEdgeHandleWire,
    },
    OutEdgeRemoved {
        source_handle: MigrationEdgeHandleWire,
    },
    OutEdgePayloadChanged {
        source_handle: MigrationEdgeHandleWire,
        payload_bytes: Vec<u8>,
    },
    OutEdgePropertySet {
        source_handle: MigrationEdgeHandleWire,
        property_id: PropertyId,
        payload_bytes: Vec<u8>,
    },
    OutEdgePropertyRemoved {
        source_handle: MigrationEdgeHandleWire,
        property_id: PropertyId,
    },
    InReverseAdded {
        source_handle: MigrationEdgeHandleWire,
        predecessor_logical_vertex_id: LogicalVertexId,
        predecessor_is_remote: bool,
        catalog_label: Option<EdgeLabelId>,
        canonical_source_handle: MigrationEdgeHandleWire,
        payload_bytes: Vec<u8>,
    },
    InReverseRemoved {
        source_handle: MigrationEdgeHandleWire,
    },
    InReverseValueChanged {
        source_handle: MigrationEdgeHandleWire,
        payload_bytes: Vec<u8>,
    },
    InReversePropertySet {
        source_handle: MigrationEdgeHandleWire,
        property_id: PropertyId,
        payload_bytes: Vec<u8>,
    },
    InReversePropertyRemoved {
        source_handle: MigrationEdgeHandleWire,
        property_id: PropertyId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MigrationJournalEntry {
    pub logical_vertex_id: LogicalVertexId,
    pub epoch: u64,
    pub seq: u64,
    pub op: MigrationJournalOp,
}

/// Vertex labels and properties captured on the source before staging.
#[derive(Clone, Debug, PartialEq, Eq, Default, CandidType, Serialize, Deserialize)]
pub struct MigrationMetadataSnapshot {
    pub labels: Vec<VertexLabelId>,
    pub properties: Vec<super::migration::ExportedProperty>,
}

/// Result of starting migration on a shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MigrationStartResult {
    pub logical_vertex_id: LogicalVertexId,
    pub epoch: u64,
    pub local_vertex_id: LocalVertexId,
    pub metadata_snapshot: MigrationMetadataSnapshot,
}

/// Status snapshot for ops/debug.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MigrationStatus {
    pub item: Option<MigrationItem>,
    pub local_state: Option<VertexMigrationState>,
    pub journal_len: u64,
    pub ready_for_cutover: bool,
}

/// What [`MigrationReconcileReport`] did when reconciling local state with the router.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MigrationReconcileAction {
    NoOp,
    RemovedStaleEpoch { epoch: u64 },
    CleanedOrphanArtifacts { epoch: u64 },
    InstalledForwardingStub,
    RebuiltQueueItem,
    AwaitingManualIntervention,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MigrationReconcileReport {
    pub action: MigrationReconcileAction,
}

/// Phases for source-local cleanup of a [`VertexMigrationState::ForwardingStub`] row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum PruneMigratedSourcePhase {
    ClearSourceOutEdges,
    ClearSourceInReverse,
    ClearSourceVertexPayload,
    Done,
}

/// Resumable work item: clear redundant CSR payload on a forwarding stub without
/// mutating neighboring vertices' canonical adjacency.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PruneMigratedSourceItem {
    pub logical_vertex_id: LogicalVertexId,
    pub source_local_vertex_id: LocalVertexId,
    pub epoch: u64,
    pub phase: PruneMigratedSourcePhase,
    pub bulk_limit: u32,
    pub removed_edges: u64,
}

impl PruneMigratedSourceItem {
    pub const INITIAL_BULK_LIMIT: u32 = 64;
    pub const MIN_BULK_LIMIT: u32 = 1;
    pub const MAX_BULK_LIMIT: u32 = 4096;

    pub fn new(
        logical_vertex_id: LogicalVertexId,
        source_local_vertex_id: LocalVertexId,
        epoch: u64,
    ) -> Self {
        Self {
            logical_vertex_id,
            source_local_vertex_id,
            epoch,
            phase: PruneMigratedSourcePhase::ClearSourceOutEdges,
            bulk_limit: Self::INITIAL_BULK_LIMIT,
            removed_edges: 0,
        }
    }
}

/// Destination shard begins staging row (after source `migration_start`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MigrationStagingArgs {
    pub logical_vertex_id: LogicalVertexId,
    pub epoch: u64,
    pub source_shard_id: ShardId,
    pub source_local_vertex_id: LocalVertexId,
    pub metadata_snapshot: MigrationMetadataSnapshot,
}

/// Chunk of copied adjacency applied on the destination shard.
#[derive(Clone, Debug, PartialEq, Eq, Default, CandidType, Serialize, Deserialize)]
pub struct MigrationApplyChunk {
    pub logical_vertex_id: LogicalVertexId,
    pub epoch: u64,
    pub target_local_vertex_id: LocalVertexId,
    pub out_edges: Vec<super::migration::ExportedOutEdge>,
    /// Parallel to [`Self::out_edges`]: source-side handles for handle-map wiring.
    pub out_edge_source_handles: Vec<MigrationEdgeHandleWire>,
    pub in_reverse_edges: Vec<ExportedInReverseEdge>,
    /// Journal ops replicated from the source shard during drain.
    pub journal_entries: Vec<MigrationJournalEntry>,
}

/// Derived reverse adjacency entry copied into `X.i`.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExportedInReverseEdge {
    pub catalog_label: Option<EdgeLabelId>,
    pub payload_bytes: Vec<u8>,
    pub predecessor_logical_vertex_id: LogicalVertexId,
    pub predecessor_is_remote: bool,
    pub source_reverse_handle: MigrationEdgeHandleWire,
    pub canonical_source_handle: MigrationEdgeHandleWire,
    pub properties: Vec<super::migration::ExportedProperty>,
}

macro_rules! impl_candid_storable {
    ($ty:ty) => {
        impl Storable for $ty {
            fn to_bytes(&self) -> Cow<'_, [u8]> {
                Cow::Owned(Encode!(self).expect(concat!("encode ", stringify!($ty))))
            }

            fn into_bytes(self) -> Vec<u8> {
                Encode!(&self).expect(concat!("encode ", stringify!($ty)))
            }

            fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
                Decode!(bytes.as_ref(), Self).expect(concat!("decode ", stringify!($ty)))
            }

            const BOUND: Bound = Bound::Unbounded;
        }
    };
}

impl_candid_storable!(MigrationMetadataSnapshot);
impl_candid_storable!(VertexMigrationState);
impl_candid_storable!(MigrationItem);
impl_candid_storable!(MigrationJournalEntry);
impl_candid_storable!(MigrationEdgeHandleWire);
impl_candid_storable!(PruneMigratedSourcePhase);
impl_candid_storable!(PruneMigratedSourceItem);
