//! Router-owned public durable bulk-load workflow (ADR 0057).
//!
//! The parent and receipt-map transitions live in the Router store facade. This module owns only
//! public command validation, graph-request construction, pinned-shard dispatch, and public status
//! canonical work is dispatched only from an explicitly submitted client command.

use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId, ShardId};
use gleaph_graph_kernel::plan_exec::{
    GraphOrderedEdgeBatchResult, GraphOrderedEdgeBatchResultV1, GraphOrderedVertexBatchResult,
    GraphOrderedVertexBatchResultV1, MutationId, MutationJournalState, MutationLifecyclePhase,
    OrderedBatchExecutionModeV1, OrderedEdgeBatchGraphArgs, OrderedEdgeBatchGraphArgsV1,
    OrderedMutationRetirementAck, OrderedMutationRetirementAckV1, OrderedMutationRetirementArgs,
    OrderedMutationRetirementArgsV1, OrderedVertexBatchGraphArgs, OrderedVertexBatchGraphArgsV1,
    OrderedVertexMutationRetirementAck, OrderedVertexMutationRetirementAckV1,
    OrderedVertexMutationRetirementArgs, OrderedVertexMutationRetirementArgsV1, ShardEventSeq,
};
use ic_cdk::api::{msg_caller, time};

use crate::facade::stable::bulk_load::{
    BulkLoadChunkEnvelopeV1, BulkLoadChunkProgressV1, BulkLoadChunkReceiptRecordV1,
    BulkLoadGraphReceiptV1, BulkLoadGraphRequestV1,
};
use crate::facade::stable::label_stats::{
    BulkLoadCoordinatorV1, BulkLoadLifecycleV1, BulkLoadTargetV1, ClientMutationKey,
    RouterMutationPayloadV1, RouterMutationRecord, RouterMutationRequestIdentityV1,
};
use crate::facade::store::RouterStore;
use crate::facade::store::bulk_load::{
    BulkLoadStartAdmission, BulkLoadUpdateAdmission, BulkLoadUpdateDispatchGate,
};
use crate::graph_client::{
    execute_ordered_edge_batch_on_graph, execute_ordered_vertex_batch_on_graph,
    get_mutation_journal_entry, retire_ordered_mutation_on_graph,
    retire_ordered_vertex_mutation_on_graph,
};
use crate::index_lookup::RouterIndexLookup;
use crate::state::RouterError;
use crate::types::{
    AtomicInsertEndpointV1, AtomicInsertOperationV1, AtomicInsertReceiptV1, AtomicInsertRequest,
    AtomicInsertRequestV1, BulkLoadChunkReceiptV1, BulkLoadChunkV1, BulkLoadCommand,
    BulkLoadEndpointV1, BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage,
    BulkLoadUpdateV1,
};
use gleaph_gql::{Value, value_to_index_key_bytes};

fn invalid(message: impl Into<String>) -> RouterError {
    RouterError::InvalidArgument(message.into())
}

fn bulk_parent(record: &RouterMutationRecord) -> Result<&BulkLoadCoordinatorV1, RouterError> {
    if !matches!(
        record.as_v1().request_identity,
        RouterMutationRequestIdentityV1::BulkLoadJob
    ) {
        return Err(RouterError::Conflict(
            "client_bulk_key belongs to a different mutation family".into(),
        ));
    }
    match record.payload() {
        RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) => {
            coordinator
                .validate()
                .unwrap_or_else(|error| panic!("invalid durable bulk-load coordinator: {error}"));
            Ok(coordinator)
        }
        _ => panic!("bulk-load identity/payload family mismatch in durable Router record"),
    }
}

fn bulk_record(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_key: &str,
) -> Result<RouterMutationRecord, RouterError> {
    let key = ClientMutationKey::new(caller, graph_id, client_key.to_owned());
    let record = store
        .router_mutation_record(&key)
        .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
    bulk_parent(&record)?;
    Ok(record)
}

fn public_state(coordinator: &BulkLoadCoordinatorV1) -> BulkLoadPublicStateV1 {
    match &coordinator.lifecycle {
        BulkLoadLifecycleV1::Open => BulkLoadPublicStateV1::Open,
        BulkLoadLifecycleV1::AppendPending { .. } => BulkLoadPublicStateV1::AppendPending,
        BulkLoadLifecycleV1::FinalizePending { .. } => BulkLoadPublicStateV1::FinalizePending,
        BulkLoadLifecycleV1::AbortPending { .. } => BulkLoadPublicStateV1::AbortPending,
        BulkLoadLifecycleV1::Completed => BulkLoadPublicStateV1::Completed,
        BulkLoadLifecycleV1::Aborted => BulkLoadPublicStateV1::Aborted,
        BulkLoadLifecycleV1::Failed { reason } => BulkLoadPublicStateV1::Failed {
            reason: reason.clone(),
        },
    }
}

fn terminal_expiry(terminal_at_ns: Option<u64>) -> Option<u64> {
    terminal_at_ns.map(|at| at.saturating_add(crate::facade::store::CLIENT_MUTATION_KEY_TTL_NS))
}

fn target_from_latest_shard(
    store: &RouterStore,
    graph_id: GraphId,
) -> Result<BulkLoadTargetV1, RouterError> {
    let routing =
        crate::federation::latest_shard_routing(&store.list_live_shards_for_graph_id(graph_id)?)?;
    let shard = routing
        .into_iter()
        .next()
        .ok_or(RouterError::ShardNotRegistered)?;
    Ok(BulkLoadTargetV1 {
        shard_id: shard.shard_id,
        graph_canister: shard.graph_canister,
    })
}

fn start_target_for_key(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_bulk_key: &str,
) -> Result<BulkLoadTargetV1, RouterError> {
    let key = ClientMutationKey::new(caller, graph_id, client_bulk_key.to_owned());
    match store.router_mutation_record(&key) {
        Some(record) => Ok(bulk_parent(&record)?.target.clone()),
        None => target_from_latest_shard(store, graph_id),
    }
}

fn atomic_request_from_chunk(
    graph_name: Option<String>,
    client_bulk_key: &str,
    chunk: &BulkLoadChunkV1,
) -> Result<AtomicInsertRequest, RouterError> {
    let operations = match chunk {
        BulkLoadChunkV1::Vertices(items) => items
            .iter()
            .cloned()
            .map(AtomicInsertOperationV1::Vertex)
            .collect(),
        BulkLoadChunkV1::Edges(items) => items
            .iter()
            .cloned()
            .map(|item| {
                let source = existing_endpoint_bytes(item.source)?;
                let target = existing_endpoint_bytes(item.target)?;
                Ok(AtomicInsertOperationV1::Edge(
                    crate::types::AtomicInsertEdgeV1 {
                        source: AtomicInsertEndpointV1::Existing(source),
                        target: AtomicInsertEndpointV1::Existing(target),
                        directed: item.directed,
                        edge_label_name: item.edge_label_name,
                        inline_property: item.inline_property,
                        initial_edge_properties: item.initial_edge_properties,
                    },
                ))
            })
            .collect::<Result<Vec<_>, RouterError>>()?,
        BulkLoadChunkV1::Updates(_) => {
            return Err(invalid(
                "bulk-load update chunks never build ordered atomic-insert requests",
            ));
        }
    };
    Ok(AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: client_bulk_key.to_owned(),
        graph_name,
        operations,
    }))
}

/// Extract the encoded existing vertex ID from a bulk-load endpoint. Property-based endpoints
/// must have been resolved by [`resolve_by_property_endpoints`] before the graph request is
/// built; reaching this point with one is a Router invariant violation and fails closed.
fn existing_endpoint_bytes(endpoint: BulkLoadEndpointV1) -> Result<Vec<u8>, RouterError> {
    match endpoint {
        BulkLoadEndpointV1::Existing(bytes) => Ok(bytes),
        BulkLoadEndpointV1::ByProperty(_) => Err(invalid(
            "bulk-load chunk reached graph-request construction with an unresolved property endpoint",
        )),
    }
}

/// Distinct `(vertex_label, property_name, value)` reference within one edge chunk.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ByPropertyRef {
    vertex_label: String,
    property_name: String,
    value: Vec<u8>,
}

/// Resolve every `ByProperty` edge endpoint in the chunk through the graph property index.
/// The whole candidate chunk is rejected before any operation
/// executes when any endpoint is missing or non-unique, or when the required converged property
/// index on `(vertex_label, property_name)` does not exist. Endpoints are grouped by
/// `(label, property)` and resolved with one batched equality request per group (values
/// deduplicated), following the index resume cursor until every bucket is answered. The durable
/// child row later stores the resolved graph request, so replay never re-resolves. Chunks
/// without `ByProperty` endpoints are returned unchanged and never touch the index.
async fn resolve_by_property_endpoints(
    store: &RouterStore,
    graph_id: GraphId,
    encoding_key: &ElementIdEncodingKey,
    chunk: &BulkLoadChunkV1,
) -> Result<BulkLoadChunkV1, RouterError> {
    let BulkLoadChunkV1::Edges(items) = chunk else {
        return Ok(chunk.clone());
    };
    let has_by_property = items.iter().any(|item| {
        matches!(item.source, BulkLoadEndpointV1::ByProperty(_))
            || matches!(item.target, BulkLoadEndpointV1::ByProperty(_))
    });
    if !has_by_property {
        return Ok(chunk.clone());
    }

    let mut distinct = BTreeSet::new();
    for item in items {
        for endpoint in [&item.source, &item.target] {
            if let BulkLoadEndpointV1::ByProperty(property) = endpoint {
                distinct.insert(ByPropertyRef {
                    vertex_label: property.vertex_label.clone(),
                    property_name: property.property_name.clone(),
                    value: property.value.clone(),
                });
            }
        }
    }
    let resolved = resolve_property_refs(store, graph_id, encoding_key, &distinct).await?;

    let items = items
        .iter()
        .cloned()
        .map(|mut item| {
            item.source = resolve_endpoint(item.source, &resolved)?;
            item.target = resolve_endpoint(item.target, &resolved)?;
            Ok(item)
        })
        .collect::<Result<Vec<_>, RouterError>>()?;
    Ok(BulkLoadChunkV1::Edges(items))
}

/// Resolve a set of distinct `(vertex_label, property_name, index-key value)` references
/// through the graph property index. The whole candidate set is rejected when any value is
/// missing or non-unique, or when the required converged property index on
/// `(vertex_label, property_name)` does not exist. References are grouped by `(label,
/// property)` and resolved with one batched equality request per group, following the index
/// resume cursor until every bucket is answered. Shared by edge-endpoint and update-match-key
/// resolution so both reject identically before any operation executes.
async fn resolve_property_refs(
    store: &RouterStore,
    graph_id: GraphId,
    encoding_key: &ElementIdEncodingKey,
    distinct: &BTreeSet<ByPropertyRef>,
) -> Result<BTreeMap<ByPropertyRef, Vec<u8>>, RouterError> {
    let mut groups: BTreeMap<(String, String), Vec<Vec<u8>>> = BTreeMap::new();
    for reference in distinct {
        groups
            .entry((
                reference.vertex_label.clone(),
                reference.property_name.clone(),
            ))
            .or_default()
            .push(reference.value.clone());
    }

    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    let lookup = RouterIndexLookup::from_shards(graph_id, &shards).map_err(invalid)?;
    let mut resolved: BTreeMap<ByPropertyRef, Vec<u8>> = BTreeMap::new();
    for ((vertex_label, property_name), values) in groups {
        let label_id = store.lookup_vertex_label_id(graph_id, &vertex_label)?;
        let property_id = store.lookup_property_id(graph_id, &property_name)?;
        let physical_index_id =
            crate::facade::stable::indexed_catalog::active_vertex_physical_index(
                graph_id,
                Some(label_id),
                property_id,
            )?;
        let per_value = lookup
            .batch_equal_per_value(
                physical_index_id,
                property_id.raw(),
                values,
                // Uniqueness detection needs at most two postings per bucket.
                2,
            )
            .await
            .map_err(invalid)?;
        let group_resolved = classify_resolved_values(&vertex_label, &property_name, per_value)?;
        for (value, hit) in group_resolved {
            let encoded = gleaph_graph_kernel::federation::encode_global_vertex_id(
                encoding_key,
                GlobalVertexId::new(hit.shard_id, hit.vertex_id),
            )
            .0
            .to_vec();
            resolved.insert(
                ByPropertyRef {
                    vertex_label: vertex_label.clone(),
                    property_name: property_name.clone(),
                    value,
                },
                encoded,
            );
        }
    }
    Ok(resolved)
}

/// Replace a resolved `ByProperty` endpoint with its encoded `Existing` vertex ID.
fn resolve_endpoint(
    endpoint: BulkLoadEndpointV1,
    resolved: &BTreeMap<ByPropertyRef, Vec<u8>>,
) -> Result<BulkLoadEndpointV1, RouterError> {
    match endpoint {
        BulkLoadEndpointV1::Existing(bytes) => Ok(BulkLoadEndpointV1::Existing(bytes)),
        BulkLoadEndpointV1::ByProperty(property) => {
            let reference = ByPropertyRef {
                vertex_label: property.vertex_label,
                property_name: property.property_name,
                value: property.value,
            };
            let encoded = resolved.get(&reference).ok_or_else(|| {
                RouterError::Internal(
                    "resolved property endpoint map is missing a reference".into(),
                )
            })?;
            Ok(BulkLoadEndpointV1::Existing(encoded.clone()))
        }
    }
}

/// One match-key-resolved update row ready for GQL journal execution. `ordinal` is the authored
/// row index inside the candidate batch, which is also the durable per-row journal ordinal: a
/// retry that resumes after a committed prefix therefore reuses the identical journal key.
struct ResolvedUpdateRow {
    ordinal: usize,
    vertex_id: Vec<u8>,
    vertex_label: String,
    property_name: String,
    set_properties: Vec<(String, Value)>,
    remove_properties: Vec<String>,
}

/// Decode the uncommitted rows, resolving match keys only on initial admission. A pending child
/// supplies its saved identities instead; neither its committed prefix nor its unwritten suffix
/// may be retargeted by property changes. The candidate batch is
/// rejected before any row executes when any initial match key is missing or non-unique, when a
/// match value is not index-comparable, or when the required converged property index on
/// `(vertex_label, property_name)` does not exist. Catalog names (labels
/// plus match, SET, and REMOVE property names) are additionally pre-checked against the
/// label and property catalogs so unknown names reject before admission; a never-registered
/// REMOVE name therefore rejects with `NotFound` exactly like the seed-time `ReadExisting`
/// resolution single-statement GQL `REMOVE` performs, while removing a registered-but-absent
/// value is a no-op success inside the Graph executor. SET values decode with the row so malformed binaries never reach dispatch.
async fn resolve_update_match_keys(
    store: &RouterStore,
    graph_id: GraphId,
    encoding_key: &ElementIdEncodingKey,
    from_ordinal: usize,
    items: &[BulkLoadUpdateV1],
    pinned: Option<&[Vec<u8>]>,
) -> Result<Vec<ResolvedUpdateRow>, RouterError> {
    let mut label_names = BTreeSet::new();
    let mut property_names = BTreeSet::new();
    for item in items {
        label_names.insert(item.vertex_label.clone());
        property_names.insert(item.property_name.clone());
        for set in &item.set_properties {
            property_names.insert(set.property_name.clone());
        }
        // REMOVE names join the same pre-check: a never-registered name rejects the chunk
        // here with `NotFound` (identical outcome to the seed-time `ReadExisting` resolution
        // single-statement GQL `REMOVE` performs, but before admission so a bad name can never
        // strand an admitted-but-incomplete child or surface as a partial commit).
        for name in &item.remove_properties {
            property_names.insert(name.clone());
        }
    }
    store.resolve_ordered_vertex_catalogs(graph_id, label_names, property_names)?;

    let mut distinct = BTreeSet::new();
    let mut decoded: Vec<(usize, ByPropertyRef, Vec<(String, Value)>, Vec<String>)> =
        Vec::with_capacity(items.len().saturating_sub(from_ordinal));
    for (ordinal, item) in items.iter().enumerate().skip(from_ordinal) {
        let match_value = Value::from_binary_bytes(&item.match_value).map_err(|error| {
            invalid(format!(
                "bulk-load update match value is not a binary-encoded GQL value: {error}"
            ))
        })?;
        let Some(index_key) = value_to_index_key_bytes(&match_value).map_err(|error| {
            invalid(format!(
                "bulk-load update match value is not supported by property index keys: {error}"
            ))
        })?
        else {
            return Err(invalid(
                "bulk-load update match value is not index-comparable",
            ));
        };
        let mut set_properties = Vec::with_capacity(item.set_properties.len());
        for set in &item.set_properties {
            let value = Value::from_binary_bytes(&set.value).map_err(|error| {
                invalid(format!(
                    "bulk-load update SET value for {} is not a binary-encoded GQL value: {error}",
                    set.property_name
                ))
            })?;
            set_properties.push((set.property_name.clone(), value));
        }
        let reference = ByPropertyRef {
            vertex_label: item.vertex_label.clone(),
            property_name: item.property_name.clone(),
            value: index_key,
        };
        distinct.insert(reference.clone());
        decoded.push((
            ordinal,
            reference,
            set_properties,
            item.remove_properties.clone(),
        ));
    }
    let resolved = if pinned.is_none() {
        resolve_property_refs(store, graph_id, encoding_key, &distinct).await?
    } else {
        BTreeMap::new()
    };
    decoded
        .into_iter()
        .map(|(ordinal, reference, set_properties, remove_properties)| {
            let vertex_id = match pinned {
                Some(ids) => ids.get(ordinal),
                None => resolved.get(&reference),
            }
            .cloned()
            .ok_or_else(|| {
                RouterError::Internal("resolved update vertex identity missing".into())
            })?;
            Ok(ResolvedUpdateRow {
                ordinal,
                vertex_id,
                vertex_label: reference.vertex_label,
                property_name: reference.property_name,
                set_properties,
                remove_properties,
            })
        })
        .collect()
}

/// Quote a label or property name for embedding in a generated GQL statement.
fn quote_gql_name(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Outcome evidence from the row's existing ADR 0029 Router saga and Graph journal.
/// Routing/CanonicalPending records require a Graph read; a failed read leaves the row Unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateRowOutcome {
    /// No dispatch exists, routing was released, or the exact-target journal completed with 0.
    /// This is safe to abandon only after the parent has closed permission to start new rows.
    NotWritten,
    /// The row's canonical write is durable. Either the Router confirmed it, or the Graph's own
    /// journal entry proves it landed even though the Router lost the confirmation.
    Committed,
    /// A dispatch may have committed and the outcome is not settled yet. `Append` settles it by
    /// re-running the *same* row key (journal-first idempotent, so a re-dispatch replays rather
    /// than re-applies), while `Abort` must not terminalize on it.
    Unresolved,
    /// The Router records that a dispatch may be outstanding but cannot read the journal that
    /// would settle it (shard stopped, transport fault). Nothing may be re-dispatched, re-resolved,
    /// or terminalized until a later attempt can read the journal.
    Unknown,
}

/// Classify one update row from the durable owners that already record its outcome.
///
/// The Router record is authoritative for three cases, and its absence is itself the ownership
/// proof that matters most: an ADR 0029 saga record is persisted *before* the first dispatch
/// await, so a row with no record cannot have reached the Graph. `Failed` means routing was
/// released without a durable dispatch envelope (or the mutation is terminally failed), which
/// likewise proves no canonical write. Only `CanonicalPending`/`Routing` are genuinely ambiguous,
/// and there the Graph's own mutation journal decides. For exact-target execution, a completed
/// receipt proves application with count 1 or a write-free outcome with count 0.
async fn classify_update_row(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    row_key: &str,
) -> Result<UpdateRowOutcome, RouterError> {
    let key = ClientMutationKey::new(caller, graph_id, row_key.to_owned());
    let Some(record) = store.router_mutation_record(&key) else {
        return Ok(UpdateRowOutcome::NotWritten);
    };
    match record.lifecycle_phase() {
        MutationLifecyclePhase::Failed => return Ok(UpdateRowOutcome::NotWritten),
        MutationLifecyclePhase::CanonicalCommitted
        | MutationLifecyclePhase::ProjectionPending
        | MutationLifecyclePhase::Completed => {
            let count = match record.as_v1().completed_row_count {
                Some(count) => count,
                None => match record.shards() {
                    [shard] if shard.completed() => shard.row_count(),
                    _ => {
                        return Err(RouterError::Internal(
                            "exact vertex mutation lacks its scalar outcome".into(),
                        ));
                    }
                },
            };
            return applied_target_outcome(count);
        }
        MutationLifecyclePhase::Routing | MutationLifecyclePhase::CanonicalPending => {}
    }
    let mut journal_observed = false;
    for shard in record.shards() {
        let Ok(entry) =
            get_mutation_journal_entry(shard.graph_canister(), record.as_v1().mutation_id).await
        else {
            // The journal is unreadable (shard stopped, transport fault, ...). The Router cannot
            // prove the write's absence and cannot drive the row either, so it reports a
            // retryable busy state instead of re-dispatching, re-resolving, or terminalizing.
            // The underlying fault remains visible on the row's own saga record.
            return Ok(UpdateRowOutcome::Unknown);
        };
        journal_observed = true;
        if let Some(entry) = entry
            && matches!(entry.state(), MutationJournalState::Completed)
        {
            return applied_target_outcome(entry.row_count());
        }
    }
    if !journal_observed {
        // A routing reservation may still be resolving its shard. Its owner has permission
        // to dispatch, so absence of an envelope is not yet proof of a write-free outcome.
        return Ok(UpdateRowOutcome::Unresolved);
    }
    Ok(UpdateRowOutcome::Unresolved)
}

fn applied_target_outcome(count: u64) -> Result<UpdateRowOutcome, RouterError> {
    match count {
        0 => Ok(UpdateRowOutcome::NotWritten),
        1 => Ok(UpdateRowOutcome::Committed),
        _ => Err(RouterError::Internal(
            "exact vertex mutation reported more than one target".into(),
        )),
    }
}

/// Settlement of an update chunk's authored rows, derived purely from per-row journal outcomes so
/// the decision is owned in one place and is exhaustively testable.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdateSettlement {
    /// Exclusive end of the committed prefix, including rows recorded before this settlement.
    committed: usize,
    /// Rows whose Router record exists but whose outcome is not yet settled. `Append` may settle
    /// them by re-running the identical journal key; `Abort` must wait for that to happen.
    unresolved: Vec<usize>,
    /// Rows whose journal could not be read, so their outcome is unknown in both directions.
    /// Nothing may be dispatched or terminalized past one of these.
    unreadable: Vec<usize>,
    /// A committed row was found past a non-committed one, violating authored dispatch order.
    /// Fail closed rather than claim a prefix that skips an unaccounted row.
    committed_after_gap: bool,
}

impl UpdateSettlement {
    /// Abort may publish a receipt only when every row outside the prefix is proven unwritten.
    /// Check before changing the child or parent, so unreadable evidence leaves both pending.
    fn abort_prefix(&self) -> Result<usize, RouterError> {
        if !self.unresolved.is_empty() || !self.unreadable.is_empty() {
            return Err(RouterError::Busy {
                operation: "bulk_load.append".into(),
            });
        }
        if self.committed_after_gap {
            return Err(RouterError::Internal(
                "bulk-load abort found a committed update row beyond a non-committed one".into(),
            ));
        }
        Ok(self.committed)
    }
}

/// Fold per-row outcomes (in authored order, starting at `from`) into a [`UpdateSettlement`].
/// Exhaustive by construction: only `NotWritten` counts as proven-write-free, `Committed` extends
/// the contiguous prefix, and everything else is unsettled.
fn settle_outcomes(
    from: usize,
    outcomes: impl IntoIterator<Item = UpdateRowOutcome>,
) -> UpdateSettlement {
    let mut committed = from;
    let mut unresolved = Vec::new();
    let mut unreadable = Vec::new();
    let mut committed_after_gap = false;
    let mut gap = false;
    for (offset, outcome) in outcomes.into_iter().enumerate() {
        let ordinal = from + offset;
        match outcome {
            UpdateRowOutcome::Committed => {
                if gap {
                    committed_after_gap = true;
                } else {
                    committed = ordinal + 1;
                }
            }
            UpdateRowOutcome::NotWritten => gap = true,
            UpdateRowOutcome::Unresolved => {
                gap = true;
                unresolved.push(ordinal);
            }
            UpdateRowOutcome::Unknown => {
                gap = true;
                unreadable.push(ordinal);
            }
        }
    }
    UpdateSettlement {
        committed,
        unresolved,
        unreadable,
        committed_after_gap,
    }
}

/// Classify rows `from..row_count` from the durable owners and fold them into a settlement.
async fn settle_update_rows(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_bulk_key: &str,
    chunk_index: u32,
    from: usize,
    row_count: usize,
) -> Result<UpdateSettlement, RouterError> {
    let mut outcomes = Vec::with_capacity(row_count.saturating_sub(from));
    for ordinal in from..row_count {
        let row_key = update_row_key(client_bulk_key, chunk_index, ordinal);
        outcomes.push(classify_update_row(store, caller, graph_id, &row_key).await?);
    }
    Ok(settle_outcomes(from, outcomes))
}

/// Durable per-row journal key for one authored row of one bulk-load update chunk. Shared by
/// dispatch and by journal classification so the two can never disagree.
fn update_row_key(client_bulk_key: &str, chunk_index: u32, ordinal: usize) -> String {
    format!("{client_bulk_key}:{chunk_index}:u{ordinal}")
}

/// Execute every resolved update row through the durable per-row GQL mutation journal and
/// complete the child row. Match-key failures reject the whole candidate batch before admission.
/// After admission one row is the atomic unit and the chunk is a durable committed prefix: each
/// committed row advances `updated_row_count` on the pending child before the next row
/// dispatches, so a later row error leaves the job resumable at that prefix instead of reporting
/// a prefix that excludes writes the chunk already applied. `Abort` deliberately refuses to
/// terminalize such a chunk (see `abort_bulk_load`) until its rows are accounted for. Per-row journal keys make
/// re-execution of an in-flight chunk converge without double application (SET is an absolute
/// assignment, REMOVE of an absent property is a no-op), and the statement is scoped to the
/// job's resolved `graph_id` rather than the caller's HOME/session graph.
async fn append_bulk_load_updates(
    graph_name: Option<String>,
    client_bulk_key: String,
    chunk_index: u32,
    chunk_fingerprint: [u8; 32],
    items: Vec<BulkLoadUpdateV1>,
) -> Result<BulkLoadResponse, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
    let parent_mutation_id = record.as_v1().mutation_id;
    let coordinator = bulk_parent(&record)?.clone();
    if coordinator.receipt_gc_cursor.is_some() {
        return Err(RouterError::Conflict(
            "client_bulk_key expired while bulk-load receipt GC is active".into(),
        ));
    }
    let encoding_key = store.graph_element_id_encoding_key(graph_id)?;
    let existing_child = store.bulk_load_chunk_receipt(parent_mutation_id, chunk_index);
    if let Some(child) = &existing_child {
        if child.chunk_fingerprint != chunk_fingerprint {
            return Err(RouterError::Conflict(
                "bulk-load chunk fingerprint conflicts with the durable row".into(),
            ));
        }
        if child.progress.is_completed() {
            let updated_row_count = child.updated_row_count.ok_or_else(|| {
                RouterError::Internal("completed update child lacks its receipt count".into())
            })?;
            return Ok(BulkLoadResponse::Updated {
                chunk_index,
                next_offset: u32::try_from(updated_row_count).map_err(|_| {
                    RouterError::Internal("bulk-load update row count exceeds u32".into())
                })?,
                updated_row_count,
            });
        }
    }

    // A retry resumes at the durable prefix, and the durable per-row journal settles it: a row
    // whose write is already durable advances the prefix without being re-resolved (a prefix row
    // may have changed its own match property) and without being re-dispatched, so the per-row
    // journal ordinal and the authored payload stay identical. Admitted suffix rows reuse saved
    // identities too; only initial admission resolves match keys.
    let recorded_prefix = existing_child
        .as_ref()
        .and_then(|child| child.updated_row_count)
        .unwrap_or(0) as usize;
    let settlement = settle_update_rows(
        &store,
        caller,
        graph_id,
        &client_bulk_key,
        chunk_index,
        recorded_prefix,
        items.len(),
    )
    .await?;
    let committed_prefix = settlement.committed;
    if settlement.committed_after_gap {
        return Err(RouterError::Internal(
            "bulk-load update chunk has a committed row beyond a non-committed one".into(),
        ));
    }
    if committed_prefix > recorded_prefix {
        store.record_bulk_load_update_progress(
            caller,
            graph_id,
            &client_bulk_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            u64::try_from(committed_prefix).map_err(|_| {
                RouterError::Internal("bulk-load update row count exceeds u64".into())
            })?,
        )?;
    }
    if !settlement.unreadable.is_empty() {
        // A row's journal could not be read, so its outcome is unknown in both directions:
        // dispatching it could double-apply and leaving it could hide a write. Retry later.
        return Err(RouterError::Busy {
            operation: "bulk_load.append".into(),
        });
    }
    let identities = existing_child
        .as_ref()
        .map(|child| {
            let ids = child.resolved_update_vertex_ids.clone().ok_or_else(|| {
                RouterError::Internal("pending update child lacks its target identities".into())
            })?;
            if ids.len() != items.len() {
                return Err(RouterError::Internal(
                    "pending update identities do not match the authored chunk".into(),
                ));
            }
            Ok(ids)
        })
        .transpose()?;
    let resolved = resolve_update_match_keys(
        &store,
        graph_id,
        &encoding_key,
        committed_prefix,
        &items,
        identities.as_deref(),
    )
    .await?;
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::take_bulk_update_abort_before_admission() {
        // Targets were resolved/restored; inject the competing transition before re-admission
        // of the stale child snapshot. Use the real Abort path and its journal checks.
        abort_bulk_load(graph_name.clone(), client_bulk_key.clone()).await?;
    }
    let identities =
        identities.unwrap_or_else(|| resolved.iter().map(|row| row.vertex_id.clone()).collect());
    let admission = store.admit_bulk_load_update_child(
        caller,
        graph_id,
        &client_bulk_key,
        parent_mutation_id,
        chunk_index,
        chunk_fingerprint,
        Some(identities),
    )?;
    match admission {
        BulkLoadUpdateAdmission::Granted => {}
        // The chunk finished while this caller was suspended (for example an `Abort` closed the
        // job at the prefix the owners could prove). Replay the stored receipt; do not dispatch.
        BulkLoadUpdateAdmission::AlreadyCompleted => {
            let updated_row_count = store
                .bulk_load_chunk_receipt(parent_mutation_id, chunk_index)
                .and_then(|child| child.updated_row_count)
                .ok_or_else(|| {
                    RouterError::Internal("completed update child lacks its receipt count".into())
                })?;
            return Ok(BulkLoadResponse::Updated {
                chunk_index,
                next_offset: u32::try_from(updated_row_count).map_err(|_| {
                    RouterError::Internal("bulk-load update row count exceeds u32".into())
                })?,
                updated_row_count,
            });
        }
    }
    for row in resolved.iter() {
        let row_ordinal = row.ordinal;
        match store.bulk_load_update_dispatch_gate(
            caller,
            graph_id,
            &client_bulk_key,
            parent_mutation_id,
            chunk_index,
        )? {
            BulkLoadUpdateDispatchGate::Open => {}
            // Abort is winding this chunk down. Only a row that was already dispatched may be
            // settled (re-running its identical idempotent journal key); a row that was never
            // started must not write.
            // Settling a row that was already dispatched is exactly the journal-first idempotent
            // path, so it stays allowed while the job winds down; starting a never-dispatched row
            // does not.
            BulkLoadUpdateDispatchGate::SettleOnly
                if settlement.unresolved.contains(&row_ordinal) => {}
            BulkLoadUpdateDispatchGate::SettleOnly => {
                return Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                });
            }
            BulkLoadUpdateDispatchGate::Closed => {
                return Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                });
            }
        }
        #[cfg(feature = "pocket-ic-e2e")]
        if crate::test_fault::bulk_update_row_failure_armed(row_ordinal) {
            return Err(RouterError::Internal(
                "pocket-ic-e2e injected fault: bulk-load update row failure after a committed prefix"
                    .into(),
            ));
        }
        let mut assignments = Vec::with_capacity(row.set_properties.len());
        let mut fields = Vec::with_capacity(row.set_properties.len());
        for (position, (name, value)) in row.set_properties.iter().enumerate() {
            let param = format!("s{position}");
            assignments.push(format!("v.{} = ${param}", quote_gql_name(name)));
            fields.push((param.clone(), value.clone()));
        }
        // SET and REMOVE ride one linear GQL statement so a mixed row applies atomically to
        // the matched vertex; at least one clause is present (wire validation rejects empty
        // rows) and overlap rejects at the wire boundary, so clause order is unambiguous.
        let mut statement = format!("MATCH (v:{})", quote_gql_name(&row.vertex_label));
        if !assignments.is_empty() {
            statement.push_str(&format!(" SET {}", assignments.join(", ")));
        }
        if !row.remove_properties.is_empty() {
            let removals = row
                .remove_properties
                .iter()
                .map(|name| format!("v.{}", quote_gql_name(name)))
                .collect::<Vec<_>>()
                .join(", ");
            statement.push_str(&format!(" REMOVE {removals}"));
        }
        // Retain match-property READ authorization without using its mutable value as a filter.
        // Exact-target execution counts applied inputs, independent of this RETURN projection.
        statement.push_str(&format!(" RETURN v.{}", quote_gql_name(&row.property_name)));
        let params = gleaph_gql_ic::encode_gql_params_blob(fields)
            .map_err(|error| invalid(format!("bulk-load update params encode failed: {error}")))?;
        let row_key = update_row_key(&client_bulk_key, chunk_index, row_ordinal);
        if row_key.len() > 256 {
            return Err(invalid(
                "bulk-load update row mutation key exceeds 256 bytes; use a shorter client_bulk_key",
            ));
        }
        let target = gleaph_graph_kernel::federation::decode_global_vertex_id(
            &encoding_key,
            gleaph_graph_kernel::federation::EncodedVertexId(
                row.vertex_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| RouterError::Internal("invalid saved bulk vertex ID".into()))?,
            ),
        );
        let outcome = crate::gql::gql_execute_idempotent_on_vertex(
            statement, params, row_key, graph_id, target,
        )
        .await?;
        #[cfg(feature = "pocket-ic-e2e")]
        crate::test_fault::maybe_trap_after_bulk_update_row_dispatch();
        // Only this exact-target execution mode reports applied targets, durably as 0 or 1.
        if applied_target_outcome(outcome.row_count)? != UpdateRowOutcome::Committed {
            return Err(RouterError::NotFound(
                "bulk-load update target is no longer eligible".into(),
            ));
        }
        store.record_bulk_load_update_progress(
            caller,
            graph_id,
            &client_bulk_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            u64::try_from(row_ordinal + 1).map_err(|_| {
                RouterError::Internal("bulk-load update row ordinal exceeds u64".into())
            })?,
        )?;
    }
    // Normal completion counts the whole authored batch, including the prefix already recorded.
    // This receipt does not imply that the independently committed rows were chunk-atomic.
    let updated_row_count = u64::try_from(committed_prefix + resolved.len())
        .map_err(|_| RouterError::Internal("bulk-load update row count exceeds u64".into()))?;
    store.complete_bulk_load_update_child(
        caller,
        graph_id,
        &client_bulk_key,
        parent_mutation_id,
        chunk_index,
        chunk_fingerprint,
        updated_row_count,
        time(),
    )?;
    Ok(BulkLoadResponse::Updated {
        chunk_index,
        next_offset: u32::try_from(updated_row_count)
            .map_err(|_| RouterError::Internal("bulk-load update row count exceeds u32".into()))?,
        updated_row_count,
    })
}

/// Classify one group's per-value postings: exactly one live posting resolves the value, zero is
/// missing, and more than one (or a truncated bucket) is non-unique. The whole candidate chunk is
/// rejected before any operation executes. Shared by `ByProperty` edge-insert endpoints and
/// bulk-load update match keys, so the diagnostic names the property reference rather than one
/// caller's surface. After admission, each successful row advances the
/// durable committed prefix, and a later failure leaves the child resumable at that prefix.
fn classify_resolved_values(
    vertex_label: &str,
    property_name: &str,
    per_value: Vec<crate::index_lookup::ResolvedEqualValue>,
) -> Result<BTreeMap<Vec<u8>, gleaph_graph_kernel::index::PostingHit>, RouterError> {
    let mut resolved = BTreeMap::new();
    for result in per_value {
        let unique = result.hits.len() == 1 && result.complete;
        if result.hits.is_empty() {
            return Err(invalid(format!(
                "bulk-load property reference ({vertex_label}, {property_name}) value does not resolve to a vertex"
            )));
        }
        if !unique {
            return Err(invalid(format!(
                "bulk-load property reference ({vertex_label}, {property_name}) value resolves to multiple vertices"
            )));
        }
        resolved.insert(result.value, result.hits[0]);
    }
    Ok(resolved)
}

fn build_graph_request(
    store: &RouterStore,
    graph_id: GraphId,
    graph_name: Option<String>,
    client_bulk_key: &str,
    target: &BulkLoadTargetV1,
    chunk: &BulkLoadChunkV1,
    encoding_key: &ElementIdEncodingKey,
) -> Result<(BulkLoadGraphRequestV1, [u8; 32]), RouterError> {
    let request = atomic_request_from_chunk(graph_name, client_bulk_key, chunk)?;
    let classified = request.into_classified_bulk().map_err(invalid)?;
    match classified {
        crate::types::ClassifiedAtomicInsertRequest::Vertex(request) => {
            let crate::types::OrderedVertexBatchRequest::V1(request_v1) = &request;
            let (labels, properties) = store.resolve_ordered_vertex_catalogs(
                graph_id,
                request_v1
                    .items
                    .iter()
                    .flat_map(|item| item.vertex_labels.iter().cloned()),
                request_v1.items.iter().flat_map(|item| {
                    item.initial_properties
                        .iter()
                        .map(|property| property.property_name.clone())
                }),
            )?;
            let graph_request = request
                .to_graph_request(
                    graph_id,
                    target.shard_id,
                    target.graph_canister,
                    labels,
                    properties,
                )
                .map_err(invalid)?;
            let fingerprint =
                gleaph_graph_kernel::plan_exec::ordered_vertex_batch_graph_request_fingerprint(
                    &graph_request,
                )
                .map_err(invalid)?;
            let graph_request = match graph_request {
                gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(request) => {
                    BulkLoadGraphRequestV1::Vertex(request)
                }
            };
            let (request_graph_id, shard_id, graph_canister) = graph_request.target();
            if request_graph_id != graph_id
                || shard_id != target.shard_id
                || graph_canister != target.graph_canister
            {
                return Err(RouterError::Conflict(
                    "bulk-load Graph request does not match pinned target".into(),
                ));
            }
            let _ = encoding_key;
            Ok((graph_request, fingerprint))
        }
        crate::types::ClassifiedAtomicInsertRequest::Edge(request) => {
            let endpoints = request
                .decode_same_shard_endpoints(encoding_key)
                .map_err(invalid)?;
            if endpoints
                .iter()
                .any(|(source, _target_id)| source.shard_id != target.shard_id)
            {
                return Err(invalid(
                    "bulk-load edge endpoints resolve to a different shard",
                ));
            }
            let crate::types::OrderedEdgeBatchRequest::V1(request_v1) = &request;
            let (labels, properties) = store.resolve_ordered_edge_catalogs(
                graph_id,
                request_v1
                    .items
                    .iter()
                    .map(|item| item.edge_label_name.clone()),
                request_v1.items.iter().flat_map(|item| {
                    item.initial_edge_properties
                        .iter()
                        .map(|property| property.property_name.clone())
                }),
            )?;
            let graph_request = request
                .to_graph_request(
                    graph_id,
                    target.shard_id,
                    target.graph_canister,
                    &endpoints,
                    labels,
                    properties,
                )
                .map_err(invalid)?;
            let fingerprint =
                gleaph_graph_kernel::plan_exec::ordered_edge_batch_graph_request_fingerprint(
                    &graph_request,
                )
                .map_err(invalid)?;
            let graph_request = match graph_request {
                gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(request) => {
                    BulkLoadGraphRequestV1::Edge(request)
                }
            };
            let (request_graph_id, shard_id, graph_canister) = graph_request.target();
            if request_graph_id != graph_id
                || shard_id != target.shard_id
                || graph_canister != target.graph_canister
            {
                return Err(RouterError::Conflict(
                    "bulk-load Graph request does not match pinned target".into(),
                ));
            }
            Ok((graph_request, fingerprint))
        }
        crate::types::ClassifiedAtomicInsertRequest::Mixed(_) => Err(invalid(
            "bulk-load chunks must contain only vertices or existing-ID edges",
        )),
    }
}

fn public_receipt(
    target_shard: ShardId,
    graph_receipt: &BulkLoadGraphReceiptV1,
    encoding_key: &ElementIdEncodingKey,
) -> Result<crate::types::AtomicInsertReceiptV1, RouterError> {
    let receipt = match graph_receipt {
        BulkLoadGraphReceiptV1::Edge(receipt) => crate::types::AtomicInsertReceiptV1 {
            logical_operation_count: receipt.logical_edge_count,
            logical_vertex_count: 0,
            logical_edge_count: receipt.logical_edge_count,
            allocated_vertex_ids: Vec::new(),
        },
        BulkLoadGraphReceiptV1::Vertex(receipt) => crate::types::AtomicInsertReceiptV1 {
            logical_operation_count: receipt.logical_vertex_count,
            logical_vertex_count: receipt.logical_vertex_count,
            logical_edge_count: 0,
            allocated_vertex_ids: receipt
                .allocated_vertex_ids
                .iter()
                .copied()
                .map(|local_id| {
                    gleaph_graph_kernel::federation::encode_global_vertex_id(
                        encoding_key,
                        gleaph_graph_kernel::federation::GlobalVertexId::new(
                            target_shard,
                            local_id,
                        ),
                    )
                    .0
                    .to_vec()
                })
                .collect(),
        },
    };
    receipt.validate().map_err(invalid)?;
    Ok(receipt)
}

fn projection_target(receipt: &BulkLoadGraphReceiptV1) -> Option<ShardEventSeq> {
    match receipt {
        BulkLoadGraphReceiptV1::Edge(receipt) => receipt.emitted_delta_last_seq,
        BulkLoadGraphReceiptV1::Vertex(receipt) => receipt.emitted_delta_last_seq,
    }
}

async fn dispatch_graph_child(
    graph_request: &BulkLoadGraphRequestV1,
    child_mutation_id: MutationId,
    graph_request_fingerprint: [u8; 32],
) -> Result<BulkLoadGraphReceiptV1, RouterError> {
    match graph_request {
        BulkLoadGraphRequestV1::Edge(request) => {
            let indexed_property_catalog =
                crate::facade::stable::indexed_catalog::ordered_edge_batch_catalog(request);
            let result = execute_ordered_edge_batch_on_graph(
                request.target_graph_canister,
                OrderedEdgeBatchGraphArgs::V1(OrderedEdgeBatchGraphArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                    execution_mode: OrderedBatchExecutionModeV1::Resumable,
                    indexed_property_catalog,
                    request: gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(
                        request.clone(),
                    ),
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            match result {
                GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(
                    receipt,
                )) => Ok(BulkLoadGraphReceiptV1::Edge(receipt)),
                GraphOrderedEdgeBatchResult::V1(
                    GraphOrderedEdgeBatchResultV1::MutationRetired { .. },
                ) => Err(invalid(
                    "bulk-load Graph returned a retired child without a receipt",
                )),
            }
        }
        BulkLoadGraphRequestV1::Vertex(request) => {
            let indexed_property_catalog =
                crate::facade::stable::indexed_catalog::ordered_vertex_batch_catalog(request);
            let result = execute_ordered_vertex_batch_on_graph(
                request.target_graph_canister,
                OrderedVertexBatchGraphArgs::V1(OrderedVertexBatchGraphArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                    execution_mode: OrderedBatchExecutionModeV1::Resumable,
                    indexed_property_catalog,
                    request: gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(
                        request.clone(),
                    ),
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            match result {
                GraphOrderedVertexBatchResult::V1(GraphOrderedVertexBatchResultV1::Completed(
                    receipt,
                )) => Ok(BulkLoadGraphReceiptV1::Vertex(receipt)),
                GraphOrderedVertexBatchResult::V1(
                    GraphOrderedVertexBatchResultV1::MutationRetired { .. },
                ) => Err(invalid(
                    "bulk-load Graph returned a retired child without a receipt",
                )),
            }
        }
    }
}

async fn retire_graph_child(
    graph_request: &BulkLoadGraphRequestV1,
    child_mutation_id: MutationId,
    graph_request_fingerprint: [u8; 32],
    expected_receipt: &BulkLoadGraphReceiptV1,
) -> Result<(), RouterError> {
    match graph_request {
        BulkLoadGraphRequestV1::Edge(request) => {
            let ack = retire_ordered_mutation_on_graph(
                request.target_graph_canister,
                OrderedMutationRetirementArgs::V1(OrderedMutationRetirementArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            let OrderedMutationRetirementAck::V1(OrderedMutationRetirementAckV1 {
                mutation_id,
                graph_request_fingerprint: returned_fingerprint,
                receipt,
            }) = ack;
            if mutation_id != child_mutation_id
                || returned_fingerprint != graph_request_fingerprint
                || !matches!(expected_receipt, BulkLoadGraphReceiptV1::Edge(expected) if &receipt == expected)
            {
                return Err(invalid(
                    "bulk-load edge retirement acknowledgement does not match the child receipt",
                ));
            }
        }
        BulkLoadGraphRequestV1::Vertex(request) => {
            let ack = retire_ordered_vertex_mutation_on_graph(
                request.target_graph_canister,
                OrderedVertexMutationRetirementArgs::V1(OrderedVertexMutationRetirementArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            let OrderedVertexMutationRetirementAck::V1(OrderedVertexMutationRetirementAckV1 {
                mutation_id,
                graph_request_fingerprint: returned_fingerprint,
                receipt,
            }) = ack;
            if mutation_id != child_mutation_id
                || returned_fingerprint != graph_request_fingerprint
                || !matches!(expected_receipt, BulkLoadGraphReceiptV1::Vertex(expected) if &receipt == expected)
            {
                return Err(invalid(
                    "bulk-load vertex retirement acknowledgement does not match the child receipt",
                ));
            }
        }
    }
    Ok(())
}

async fn drive_bulk_child(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_bulk_key: &str,
    parent_mutation_id: MutationId,
    chunk_index: u32,
    chunk_fingerprint: [u8; 32],
    target: &BulkLoadTargetV1,
    encoding_key: &ElementIdEncodingKey,
) -> Result<crate::types::AtomicInsertReceiptV1, RouterError> {
    loop {
        let child = store
            .bulk_load_chunk_receipt(parent_mutation_id, chunk_index)
            .ok_or_else(|| {
                RouterError::Internal("bulk-load child receipt row is missing".into())
            })?;
        if child.chunk_fingerprint != chunk_fingerprint {
            return Err(RouterError::Conflict(
                "bulk-load child fingerprint conflicts with the durable row".into(),
            ));
        }
        match child.progress {
            BulkLoadChunkProgressV1::CanonicalPending => {
                let graph_request = child.graph_request.as_ref().ok_or_else(|| {
                    RouterError::Internal("bulk-load child lacks its Graph request".into())
                })?;
                let graph_receipt = dispatch_graph_child(
                    graph_request,
                    child.child_mutation_id,
                    child.graph_request_fingerprint.ok_or_else(|| {
                        RouterError::Internal(
                            "bulk-load child lacks its Graph request fingerprint".into(),
                        )
                    })?,
                )
                .await?;
                let (request_graph_id, shard_id, graph_canister) = graph_request.target();
                if request_graph_id != graph_id
                    || shard_id != target.shard_id
                    || graph_canister != target.graph_canister
                {
                    return Err(RouterError::Internal(
                        "bulk-load child target changed after admission".into(),
                    ));
                }
                let public_receipt = public_receipt(target.shard_id, &graph_receipt, encoding_key)?;
                store.record_bulk_load_canonical_committed(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                    graph_receipt,
                    public_receipt,
                )?;
            }
            BulkLoadChunkProgressV1::CanonicalCommitted
            | BulkLoadChunkProgressV1::ProjectionPending => {
                let graph_receipt = child.graph_receipt.clone().ok_or_else(|| {
                    RouterError::Internal("bulk-load child lacks its Graph receipt".into())
                })?;
                store.record_bulk_load_projection_pending(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                )?;
                crate::gql::advance_label_stats_projection_through(
                    store,
                    graph_id,
                    target.graph_canister,
                    target.shard_id,
                    projection_target(&graph_receipt),
                )
                .await?;
                store.record_bulk_load_retirement_pending(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                )?;
            }
            BulkLoadChunkProgressV1::RetirementPending => {
                let graph_receipt = child.graph_receipt.clone().ok_or_else(|| {
                    RouterError::Internal("bulk-load child lacks its Graph receipt".into())
                })?;
                retire_graph_child(
                    child.graph_request.as_ref().ok_or_else(|| {
                        RouterError::Internal("bulk-load child lacks its Graph request".into())
                    })?,
                    child.child_mutation_id,
                    child.graph_request_fingerprint.ok_or_else(|| {
                        RouterError::Internal(
                            "bulk-load child lacks its Graph request fingerprint".into(),
                        )
                    })?,
                    &graph_receipt,
                )
                .await?;
                store.complete_bulk_load_child(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                    time(),
                )?;
            }
            BulkLoadChunkProgressV1::Completed => {
                return child.public_receipt.ok_or_else(|| {
                    RouterError::Internal("completed bulk-load child lacks public receipt".into())
                });
            }
        }
    }
}

fn start_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
) -> Result<BulkLoadResponse, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let target = start_target_for_key(&store, caller, graph_id, &client_bulk_key)?;
    match store.start_bulk_load_job(caller, graph_id, &client_bulk_key, target, time())? {
        BulkLoadStartAdmission::Created { .. } => Ok(BulkLoadResponse::Started {
            next_chunk_index: 0,
        }),
        BulkLoadStartAdmission::Replay { record } => {
            let coordinator = bulk_parent(&record)?;
            Ok(BulkLoadResponse::Started {
                next_chunk_index: coordinator.next_chunk_index,
            })
        }
    }
}

async fn append_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
    chunk_index: u32,
    chunk: BulkLoadChunkV1,
) -> Result<BulkLoadResponse, RouterError> {
    if let BulkLoadChunkV1::Updates(items) = chunk {
        let chunk_fingerprint =
            BulkLoadChunkEnvelopeV1::from_chunk(&BulkLoadChunkV1::Updates(items.clone()))
                .fingerprint()
                .map_err(invalid)?;
        return append_bulk_load_updates(
            graph_name,
            client_bulk_key,
            chunk_index,
            chunk_fingerprint,
            items,
        )
        .await;
    }
    let chunk_envelope = BulkLoadChunkEnvelopeV1::from_chunk(&chunk);
    let chunk_fingerprint = chunk_envelope.fingerprint().map_err(invalid)?;
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
    let parent_mutation_id = record.as_v1().mutation_id;
    let coordinator = bulk_parent(&record)?.clone();
    if coordinator.receipt_gc_cursor.is_some() {
        return Err(RouterError::Conflict(
            "client_bulk_key expired while bulk-load receipt GC is active".into(),
        ));
    }
    let encoding_key = store.graph_element_id_encoding_key(graph_id)?;

    if let Some(child) = store.bulk_load_chunk_receipt(parent_mutation_id, chunk_index) {
        if child.chunk_fingerprint != chunk_fingerprint {
            return Err(RouterError::Conflict(
                "bulk-load chunk fingerprint conflicts with the durable row".into(),
            ));
        }
        let receipt = drive_bulk_child(
            &store,
            caller,
            graph_id,
            &client_bulk_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            &coordinator.target,
            &encoding_key,
        )
        .await?;
        return Ok(BulkLoadResponse::Appended {
            chunk_index,
            next_offset: committed_offset(&receipt)?,
            receipt,
        });
    }

    if !matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Open)
        || chunk_index != coordinator.next_chunk_index
    {
        return Err(RouterError::Conflict(
            "bulk-load append is not the next admissible chunk".into(),
        ));
    }
    // Resolve property-based endpoints before any admission or dispatch: the whole candidate
    // chunk is rejected when any endpoint is missing or non-unique, so failures never surface as
    // a partial commit mid-chunk. The durable child row stores the resolved graph request, so
    // replay never re-resolves.
    let resolved_chunk =
        resolve_by_property_endpoints(&store, graph_id, &encoding_key, &chunk).await?;
    let (graph_request, graph_request_fingerprint) = build_graph_request(
        &store,
        graph_id,
        graph_name,
        &client_bulk_key,
        &coordinator.target,
        &resolved_chunk,
        &encoding_key,
    )?;
    let child = BulkLoadChunkReceiptRecordV1 {
        chunk_fingerprint,
        graph_request: Some(graph_request),
        graph_request_fingerprint: Some(graph_request_fingerprint),
        updated_row_count: None,
        child_mutation_id: 1,
        progress: BulkLoadChunkProgressV1::CanonicalPending,
        public_receipt: None,
        graph_receipt: None,
        resolved_update_vertex_ids: None,
        completed_at_ns: None,
    };
    store.admit_bulk_load_child(
        caller,
        graph_id,
        &client_bulk_key,
        parent_mutation_id,
        chunk_index,
        chunk_fingerprint,
        child,
    )?;
    let receipt = drive_bulk_child(
        &store,
        caller,
        graph_id,
        &client_bulk_key,
        parent_mutation_id,
        chunk_index,
        chunk_fingerprint,
        &coordinator.target,
        &encoding_key,
    )
    .await?;
    Ok(BulkLoadResponse::Appended {
        chunk_index,
        next_offset: committed_offset(&receipt)?,
        receipt,
    })
}

/// Operations of the candidate batch committed as this chunk. The public receipt counts are the
/// committed prefix (one of vertex/edge is zero), so the operation count is the offset the client
/// resumes from at `chunk_index + 1` (ADR 0060).
fn committed_offset(receipt: &crate::types::AtomicInsertReceiptV1) -> Result<u32, RouterError> {
    u32::try_from(receipt.logical_operation_count)
        .map_err(|_| RouterError::Internal("bulk-load chunk committed count exceeds u32".into()))
}

fn finalize_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
) -> Result<BulkLoadResponse, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let coordinator = store.begin_bulk_load_finalize(caller, graph_id, &client_bulk_key)?;
    let coordinator = if matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Completed) {
        coordinator
    } else {
        store.finalize_bulk_load_step(caller, graph_id, &client_bulk_key, time())?
    };
    Ok(BulkLoadResponse::FinalizeAccepted {
        state: public_state(&coordinator),
    })
}

async fn abort_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
) -> Result<BulkLoadResponse, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let coordinator = store.begin_bulk_load_abort(caller, graph_id, &client_bulk_key, time())?;
    if let BulkLoadLifecycleV1::AbortPending { active_chunk } = coordinator.lifecycle {
        let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
        let parent_mutation_id = record.as_v1().mutation_id;
        let child = store
            .bulk_load_chunk_receipt(parent_mutation_id, active_chunk)
            .ok_or_else(|| RouterError::Internal("bulk-load abort child row is missing".into()))?;
        let encoding_key = store.graph_element_id_encoding_key(graph_id)?;
        if child.updated_row_count.is_some() {
            // An update chunk's durable count is its committed prefix, and its authored payload
            // is deliberately not persisted (receipt rows stay bounded for status pagination),
            // so Abort cannot itself re-drive the unexecuted suffix. It does not need to: the
            // per-row journal already answers whether any row outside the prefix wrote.
            if !child.progress.is_completed() {
                // The authored row count is durable in the retry guard, so Abort can classify
                // every row by its derived key without the (deliberately unpersisted) payload.
                let row_count = child
                    .resolved_update_vertex_ids
                    .as_ref()
                    .map(|identities| identities.len())
                    .ok_or_else(|| {
                        RouterError::Internal(
                            "pending update child lacks its admitted row count".into(),
                        )
                    })?;
                // Settle first: a row whose write is durable but not yet recorded must move the
                // prefix forward rather than be abandoned.
                let settlement = settle_update_rows(
                    &store,
                    caller,
                    graph_id,
                    &client_bulk_key,
                    active_chunk,
                    child.updated_row_count.unwrap_or(0) as usize,
                    row_count,
                )
                .await?;
                let settled = settlement.abort_prefix()?;
                if settled > child.updated_row_count.unwrap_or(0) as usize {
                    store.record_bulk_load_update_progress(
                        caller,
                        graph_id,
                        &client_bulk_key,
                        parent_mutation_id,
                        active_chunk,
                        child.chunk_fingerprint,
                        settled as u64,
                    )?;
                }
                store.complete_bulk_load_update_child(
                    caller,
                    graph_id,
                    &client_bulk_key,
                    parent_mutation_id,
                    active_chunk,
                    child.chunk_fingerprint,
                    settled as u64,
                    time(),
                )?;
            } else {
                store.complete_bulk_load_update_child(
                    caller,
                    graph_id,
                    &client_bulk_key,
                    parent_mutation_id,
                    active_chunk,
                    child.chunk_fingerprint,
                    child.updated_row_count.unwrap_or(0),
                    time(),
                )?;
            }
        } else {
            drive_bulk_child(
                &store,
                caller,
                graph_id,
                &client_bulk_key,
                parent_mutation_id,
                active_chunk,
                child.chunk_fingerprint,
                &bulk_parent(&record)?.target,
                &encoding_key,
            )
            .await?;
        }
    }
    let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
    let coordinator = bulk_parent(&record)?;
    Ok(BulkLoadResponse::AbortAccepted {
        state: public_state(coordinator),
    })
}

/// Public update entrypoint used by `api::client::bulk_load`.
pub(crate) async fn bulk_load_public(
    command: BulkLoadCommand,
) -> Result<BulkLoadResponse, RouterError> {
    command.validate().map_err(invalid)?;
    match command {
        BulkLoadCommand::Start {
            graph_name,
            client_bulk_key,
        } => start_bulk_load(graph_name, client_bulk_key),
        BulkLoadCommand::Append {
            graph_name,
            client_bulk_key,
            chunk_index,
            chunk,
        } => append_bulk_load(graph_name, client_bulk_key, chunk_index, chunk).await,
        BulkLoadCommand::Finalize {
            graph_name,
            client_bulk_key,
        } => finalize_bulk_load(graph_name, client_bulk_key),
        BulkLoadCommand::Abort {
            graph_name,
            client_bulk_key,
        } => abort_bulk_load(graph_name, client_bulk_key).await,
    }
}

/// Public status query used by `api::client::bulk_load_status`.
pub(crate) fn bulk_load_status_public(
    graph_name: Option<String>,
    client_bulk_key: String,
    receipt_cursor: Option<u32>,
    max_receipts: u32,
) -> Result<BulkLoadStatusPage, RouterError> {
    crate::types::validate_max_receipts(max_receipts).map_err(invalid)?;
    BulkLoadCommand::Start {
        graph_name: graph_name.clone(),
        client_bulk_key: client_bulk_key.clone(),
    }
    .validate()
    .map_err(invalid)?;
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
    let coordinator = bulk_parent(&record)?;
    let cursor = receipt_cursor.unwrap_or(0);
    // Completed rows are compacted by `complete_bulk_load_child`, so status pagination decodes
    // only receipt-sized rows; a job has at most one transient in-flight child at a time.
    let rows =
        store.list_bulk_load_chunk_receipts(record.as_v1().mutation_id, cursor, max_receipts)?;
    let receipts = rows
        .iter()
        .filter_map(|(chunk_index, row)| {
            if let Some(updated_row_count) = row.updated_row_count {
                // Only a completed update chunk is public. A pending one is invisible exactly
                // like a pending insert child (whose `public_receipt` is `None`), so a client
                // resuming from this projection re-sends the whole in-flight chunk at
                // `next_chunk_index` from the boundary after the last completed chunk instead of
                // skipping the in-flight prefix and mis-slicing the payload.
                if !row.progress.is_completed() {
                    return None;
                }
                // Update chunks carry no insert receipt; report a zero receipt plus the
                // committed row count.
                return Some(BulkLoadChunkReceiptV1 {
                    chunk_index: *chunk_index,
                    receipt: AtomicInsertReceiptV1 {
                        logical_operation_count: 0,
                        logical_vertex_count: 0,
                        logical_edge_count: 0,
                        allocated_vertex_ids: Vec::new(),
                    },
                    updated_row_count,
                });
            }
            row.public_receipt
                .clone()
                .map(|receipt| BulkLoadChunkReceiptV1 {
                    chunk_index: *chunk_index,
                    receipt,
                    updated_row_count: 0,
                })
        })
        .collect::<Vec<_>>();
    let next_receipt_cursor = rows.last().and_then(|(chunk_index, _)| {
        let next = chunk_index.checked_add(1)?;
        store
            .bulk_load_has_chunk_receipt_at_or_after(record.as_v1().mutation_id, next)
            .then_some(next)
    });
    Ok(BulkLoadStatusPage {
        state: public_state(coordinator),
        next_chunk_index: coordinator.next_chunk_index,
        committed_chunk_count: coordinator.committed_chunk_count,
        completed_chunk_count: coordinator.completed_chunk_count,
        terminal_at_ns: record.as_v1().terminal_at_ns,
        expires_at_ns: terminal_expiry(record.as_v1().terminal_at_ns),
        receipts,
        next_receipt_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::store::tests::test_init_args;
    use crate::index_lookup::ResolvedEqualValue;

    /// T3: the row classifier reads the durable owners rather than inferring from an error.
    /// A released routing reservation (`Failed`) is proven write-free, a committed phase is
    /// write-durable only with an applied target, and an unreadable journal is `Unknown`.
    #[test]
    fn update_row_classifier_uses_the_owners_not_the_error() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([71; 32]);
        let graph_id = GraphId::from_raw(1);
        let key = "row-journal";

        // No router record at all: the row cannot have been dispatched, because the reservation
        // is persisted before the first dispatch await.
        assert_eq!(
            futures::executor::block_on(classify_update_row(&store, caller, graph_id, key))
                .expect("classify"),
            UpdateRowOutcome::NotWritten
        );

        // The owner's own routing release (ADR 0029 Phase 4) leaves an empty envelope and no
        // completed rows, which is the documented "no canonical write committed" state.
        let client_key = ClientMutationKey::new(caller, graph_id, key.to_owned());
        store
            .reserve_mutation_id_for_client_key(caller, graph_id, key, vec![7u8; 32])
            .expect("reserve");
        store
            .abandon_router_mutation_routing_reservation(&client_key)
            .expect("release routing");
        assert_eq!(
            futures::executor::block_on(classify_update_row(&store, caller, graph_id, key))
                .expect("classify"),
            UpdateRowOutcome::NotWritten,
            "a released routing reservation proves no canonical write"
        );

        // A committed phase is write-durable without consulting any journal.
        let record = store
            .router_mutation_record(&client_key)
            .expect("record after reservation");
        store
            .record_router_mutation_completed_without_shards(
                &client_key,
                record.as_v1().resolved_labels.clone().unwrap_or_default(),
                record
                    .as_v1()
                    .resolved_properties
                    .clone()
                    .unwrap_or_default(),
                1,
            )
            .expect("record completion");
        assert_eq!(
            futures::executor::block_on(classify_update_row(&store, caller, graph_id, key))
                .expect("classify"),
            UpdateRowOutcome::Committed,
            "a completed row is write-durable"
        );

        // Completed is not synonymous with an applied exact target: a durable zero-effect
        // result is write-free, including after the shard envelope has been compacted.
        let zero = ClientMutationKey::new(caller, graph_id, "row-zero".into());
        store
            .reserve_mutation_id_for_client_key(caller, graph_id, "row-zero", vec![8u8; 32])
            .unwrap();
        store
            .record_router_mutation_completed_without_shards(
                &zero,
                Default::default(),
                Default::default(),
                0,
            )
            .unwrap();
        assert_eq!(
            store
                .router_mutation_record(&zero)
                .unwrap()
                .lifecycle_phase(),
            MutationLifecyclePhase::Completed
        );
        assert_eq!(
            futures::executor::block_on(classify_update_row(&store, caller, graph_id, "row-zero"))
                .unwrap(),
            UpdateRowOutcome::NotWritten
        );
        assert_eq!(
            applied_target_outcome(2),
            Err(RouterError::Internal(
                "exact vertex mutation reported more than one target".into()
            ))
        );

        // A record whose graph journal cannot be read is never write-free. Native (unit) builds
        // have no graph call, so the read fails: the classifier must report `Unknown`, and Abort
        // must therefore refuse to terminalize on it (asserted by the settlement owner).
        // A dispatched row is keyed by its authored ordinal, so the classifier and the lane must
        // agree on the derived key.
        let pending_key = update_row_key("row-journal-pending", 0, 0);
        store
            .reserve_mutation_id_for_client_key(caller, graph_id, &pending_key, vec![9u8; 32])
            .expect("reserve pending");
        store
            .record_router_mutation_shards(
                &ClientMutationKey::new(caller, graph_id, pending_key.clone()),
                gleaph_graph_kernel::plan_exec::ResolvedLabelTable::default(),
                gleaph_graph_kernel::plan_exec::ResolvedPropertyTable::default(),
                vec![
                    crate::facade::stable::label_stats::RouterMutationShardV1::new(
                        ShardId::new(0),
                        Principal::from_slice(&[0xCD; 29]),
                        None,
                    ),
                ],
            )
            .expect("persist envelope");
        assert_eq!(
            futures::executor::block_on(classify_update_row(
                &store,
                caller,
                graph_id,
                &pending_key
            ))
            .expect("classify"),
            UpdateRowOutcome::Unknown,
            "an unreadable journal must stay unknown, never write-free"
        );
        let settlement = futures::executor::block_on(settle_update_rows(
            &store,
            caller,
            graph_id,
            "row-journal-pending",
            0,
            0,
            1,
        ))
        .expect("settle");
        assert_eq!(settlement.unreadable, vec![0]);
        assert_eq!(settlement.committed, 0);
        assert!(settlement.unresolved.is_empty());
    }

    /// T2: only `NotWritten` is proven write-free. `Committed` extends the contiguous prefix,
    /// `Unresolved` must be settled by re-dispatch, `Unknown` blocks everything, and a committed
    /// row behind a gap fails closed.
    #[test]
    fn settle_outcomes_is_exhaustive_over_row_journal_states() {
        use UpdateRowOutcome::{Committed, NotWritten, Unknown, Unresolved};

        // All rows proven write-free: nothing committed, nothing to settle.
        let all_free = settle_outcomes(2, vec![NotWritten, NotWritten]);
        assert_eq!(all_free.committed, 2, "the recorded prefix is preserved");
        assert!(all_free.unresolved.is_empty());
        assert!(all_free.unreadable.is_empty());
        assert!(!all_free.committed_after_gap);

        // A proven-unwritten gap must not hide an unsettled tail. In particular, stopping at
        // the first NotWritten row or treating every non-Completed journal as write-free is unsafe.
        for tail in [NotWritten, Committed, Unresolved, Unknown] {
            let settlement = settle_outcomes(2, [NotWritten, tail]);
            assert_eq!(settlement.committed, 2);
            assert_eq!(settlement.committed_after_gap, tail == Committed);
            assert_eq!(
                settlement.unresolved,
                if tail == Unresolved { vec![3] } else { vec![] }
            );
            assert_eq!(
                settlement.unreadable,
                if tail == Unknown { vec![3] } else { vec![] }
            );
            let expected = match tail {
                NotWritten => Ok(2),
                Committed => Err(RouterError::Internal(
                    "bulk-load abort found a committed update row beyond a non-committed one"
                        .into(),
                )),
                Unresolved | Unknown => Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                }),
            };
            assert_eq!(settlement.abort_prefix(), expected, "tail {tail:?}");
        }

        // A committed tail extends the prefix; the fold starts at the recorded prefix.
        let extended = settle_outcomes(1, vec![Committed, Committed]);
        assert_eq!(extended.committed, 3);
        assert!(extended.unresolved.is_empty());
        assert!(!extended.committed_after_gap);

        // Unresolved rows are settleable by re-dispatch; Unknown rows are not. Each is reported by
        // its authored ordinal, and the prefix stops at the first non-committed row.
        for (outcome, ordinal, outcomes) in [
            (Unresolved, 0usize, vec![Unresolved, NotWritten]),
            (Unknown, 0, vec![Unknown, NotWritten]),
            (Unresolved, 1, vec![Committed, Unresolved]),
            (Unknown, 1, vec![Committed, Unknown]),
        ] {
            let settlement = settle_outcomes(0, outcomes);
            match outcome {
                Unresolved => assert_eq!(
                    settlement.unresolved,
                    vec![ordinal],
                    "{outcome:?} at {ordinal} must be the only unresolved row: {settlement:?}"
                ),
                Unknown => assert_eq!(
                    settlement.unreadable,
                    vec![ordinal],
                    "{outcome:?} at {ordinal} must be the only unreadable row: {settlement:?}"
                ),
                other => panic!("unexpected fixture state {other:?}"),
            }
            assert_eq!(
                settlement.committed, ordinal,
                "the prefix must stop at the unsettled row: {settlement:?}"
            );
            assert!(
                !settlement.committed_after_gap,
                "an unsettled row is a gap, not a committed-beyond-gap: {settlement:?}"
            );
        }

        // NotWritten then Committed violates authored dispatch order: fail closed rather
        // than claim a prefix that skips an unaccounted row.
        let torn = settle_outcomes(0, vec![NotWritten, Committed]);
        assert_eq!(torn.committed, 0);
        assert!(torn.committed_after_gap);
        assert!(torn.unresolved.is_empty() && torn.unreadable.is_empty());

        // A committed prefix followed by a write-free tail still advances the prefix.
        let prefix_then_free = settle_outcomes(0, vec![Committed, NotWritten, NotWritten]);
        assert_eq!(prefix_then_free.committed, 1);
        assert!(prefix_then_free.unresolved.is_empty());
        assert!(!prefix_then_free.committed_after_gap);

        // A committed row behind an unsettled gap is both unsettled and inconsistent, so the
        // caller must fail closed instead of choosing either interpretation.
        let behind_gap = settle_outcomes(0, vec![Unresolved, Committed]);
        assert_eq!(behind_gap.unresolved, vec![0]);
        assert!(behind_gap.committed_after_gap);
    }

    #[test]
    fn classify_resolved_values_resolves_unique_and_rejects_missing_or_non_unique() {
        let hit = |shard: u32, vertex: u32| gleaph_graph_kernel::index::PostingHit {
            shard_id: ShardId::new(shard),
            vertex_id: vertex,
        };
        let unique = ResolvedEqualValue {
            value: b"a".to_vec(),
            hits: vec![hit(0, 7)],
            complete: true,
        };
        let resolved = classify_resolved_values("Person", "email", vec![unique])
            .expect("unique value must resolve");
        assert_eq!(resolved[&b"a".to_vec()], hit(0, 7));

        let missing = ResolvedEqualValue {
            value: b"b".to_vec(),
            hits: Vec::new(),
            complete: true,
        };
        let error = classify_resolved_values("Person", "email", vec![missing])
            .expect_err("missing value must reject the whole chunk");
        assert!(error.to_string().contains("does not resolve"), "{error}");

        let non_unique = ResolvedEqualValue {
            value: b"c".to_vec(),
            hits: vec![hit(0, 1), hit(1, 2)],
            complete: true,
        };
        let error = classify_resolved_values("Person", "email", vec![non_unique])
            .expect_err("non-unique value must reject the whole chunk");
        assert!(error.to_string().contains("multiple vertices"), "{error}");

        // A truncated bucket (more postings than the limit) is non-unique even when only one
        // posting was materialized.
        let truncated = ResolvedEqualValue {
            value: b"d".to_vec(),
            hits: vec![hit(0, 3)],
            complete: false,
        };
        let error = classify_resolved_values("Person", "email", vec![truncated])
            .expect_err("truncated bucket must reject as non-unique");
        assert!(error.to_string().contains("multiple vertices"), "{error}");
    }

    #[test]
    fn start_rejects_wrong_mutation_family_before_shard_routing() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([41; 32]);
        let graph_id = GraphId::from_raw(1);
        store
            .reserve_mutation_id_for_client_key_at(
                caller,
                graph_id,
                "wrong-family",
                b"scalar-request".to_vec(),
                1,
            )
            .expect("seed scalar mutation record");

        assert_eq!(
            start_target_for_key(&store, caller, graph_id, "wrong-family"),
            Err(RouterError::Conflict(
                "client_bulk_key belongs to a different mutation family".into()
            )),
            "wrong-family lookup must win even when the graph has no shard to route to"
        );
    }
}
