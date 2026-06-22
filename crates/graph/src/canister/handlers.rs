//! Request bodies for canister methods (called from `lib.rs` ic-cdk entrypoints).

use std::collections::BTreeMap;

use crate::facade::{FederationRouting, GraphMetadata, GraphStore};
use crate::gql_execution_context::GqlExecutionContext;
use crate::gql_run::{kernel_execution_mode, run_wire_plans_last_read_row_count};
use crate::index::ic::IcPropertyIndexClient;
use crate::index::lookup::PropertyIndexLookup;
#[cfg(not(target_family = "wasm"))]
use crate::index::router::verify_shard_attachment;
use crate::plan_wire_guard::ensure_federated_seeds_for_index_anchors;
use candid::Decode;
use gleaph_gql::Value;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::wire::decode_plan_bundle;
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, GqlExecutionMode, SeedBindingsWire,
};

use super::types::GraphInitArgs;

pub async fn init(args: GraphInitArgs) {
    let federation_routing = match (args.router_canister, args.shard_id, args.index_canister) {
        (Some(router_canister), Some(shard_id), Some(index_canister)) => Some(FederationRouting {
            router_canister,
            shard_id,
            index_canister,
        }),
        #[cfg(target_family = "wasm")]
        (Some(_), Some(_), None) => ic_cdk::trap(
            "GraphInitArgs: index_canister is required with router_canister and shard_id during wasm init",
        ),
        #[cfg(not(target_family = "wasm"))]
        (Some(router_canister), Some(shard_id), None) => {
            let entry = verify_shard_attachment(
                router_canister,
                shard_id,
                args.logical_graph_name.as_deref(),
            )
            .unwrap_or_else(|e| panic!("{e}"));
            Some(FederationRouting {
                router_canister,
                shard_id,
                index_canister: entry.index_canister,
            })
        }
        (None, None, None) => None,
        (None, None, Some(_)) => {
            ic_cdk::trap("GraphInitArgs: index_canister requires router_canister and shard_id")
        }
        _ => ic_cdk::trap(
            "GraphInitArgs: router_canister, shard_id, and index_canister must be set together or omitted",
        ),
    };

    let mut metadata = GraphMetadata::default();
    metadata.set_logical_graph_name(args.logical_graph_name);
    metadata.set_federation_routing(federation_routing);

    if let Err(err) = GraphStore::new().set_metadata(metadata) {
        ic_cdk::trap(err.to_string());
    }
    // Before any router `GPL` plan decode (rkyv expr pools may contain `Value::Extension`).
    crate::facade::init_ic_gql_extensions();
    // Drains any maintenance the init path enqueued (normally none on a fresh graph).
    crate::facade::maintenance_timer::arm_if_needed();
}

/// Post-upgrade: a fresh Wasm instance loses process-global state, so re-install
/// the rkyv extension decode hook and re-arm the deferred-maintenance timer when
/// the stable queue still has pending reclamation (ADR 0020).
pub fn post_upgrade() {
    // Force stable-graph init at the upgrade boundary so a layout/version skew traps
    // here with an actionable message rather than lazily on the first query.
    crate::facade::ensure_graph_initialized();
    crate::facade::init_ic_gql_extensions();
    crate::facade::maintenance_timer::arm_if_needed();
}

pub(crate) fn decode_gql_param_map(params: Vec<u8>) -> Result<BTreeMap<String, Value>, String> {
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = canbench_rs::bench_scope("gql_ic_params_blob_decode");
    decode_gql_params_blob(&params).map_err(|e| e.to_string())
}

fn wasm_index_client_holder() -> Option<IcPropertyIndexClient> {
    GraphStore::new()
        .federation_routing()
        .map(|r| IcPropertyIndexClient {
            index_principal: r.index_canister,
            shard_id: r.shard_id,
        })
}

fn ensure_execution_mode(
    args_mode: GqlExecutionMode,
    expected: GqlExecutionMode,
    entrypoint: &str,
) -> Result<(), String> {
    if args_mode != expected {
        return Err(format!(
            "{entrypoint} requires {expected:?} mode (got {args_mode:?})"
        ));
    }
    Ok(())
}

pub async fn execute_plan_query(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    ensure_execution_mode(args.mode, GqlExecutionMode::Query, "execute_plan_query")?;
    execute_plan_impl(args).await
}

pub async fn execute_plan_update(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    ensure_execution_mode(args.mode, GqlExecutionMode::Update, "execute_plan_update")?;
    execute_plan_impl(args).await
}

async fn execute_plan_impl(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    let store = GraphStore::new();
    let routing = store
        .federation_routing()
        .ok_or("federation routing not configured")?;
    if routing.shard_id != args.target_shard_id {
        return Err(format!(
            "target_shard_id {} does not match this graph shard {}",
            args.target_shard_id, routing.shard_id
        ));
    }
    // ADR 0023 D1/D3: install the router-sourced indexed catalog for this
    // operation. The guard clears it on return, so the shard never persists
    // derived index state. Held until after plan execution and posting flush.
    let _catalog_guard = args
        .indexed_properties
        .map(crate::index::catalog_context::enter);
    let pmap = decode_gql_param_map(args.params_blob)?;
    let seeds = match args.seed_bindings_blob {
        Some(blob) => {
            let wire: SeedBindingsWire = Decode!(&blob, SeedBindingsWire)
                .map_err(|e| format!("seed_bindings decode: {e}"))?;
            Some(wire)
        }
        None => None,
    };
    crate::facade::init_ic_gql_extensions();
    let (bundle_requires_write, plans) =
        decode_plan_bundle(&args.plan_blob).map_err(|e| e.to_string())?;
    ensure_federated_seeds_for_index_anchors(
        seeds.as_ref(),
        store.federation_routing().is_some(),
        &plans,
    )
    .map_err(|e| e.0)?;
    // Router-owned index anchors: federated graph shards must not call index on read path.
    // Update plans still need the index client to flush mutation postings after local writes.
    #[cfg(target_family = "wasm")]
    let index_holder = match args.mode {
        GqlExecutionMode::Update => wasm_index_client_holder(),
        GqlExecutionMode::Query if seeds.is_some() || store.federation_routing().is_some() => None,
        GqlExecutionMode::Query => wasm_index_client_holder(),
    };
    #[cfg(target_family = "wasm")]
    let ix = index_holder.as_ref().map(|c| c as &dyn PropertyIndexLookup);
    #[cfg(not(target_family = "wasm"))]
    let ix: Option<&dyn PropertyIndexLookup> = None;

    let run = run_wire_plans_last_read_row_count(
        store,
        &plans,
        bundle_requires_write,
        &pmap,
        kernel_execution_mode(args.mode),
        ix,
        GqlExecutionContext {
            caller: None,
            resolved_labels: args.resolved_labels,
            resolved_properties: args.resolved_properties,
            element_id_encoding_key: Some(args.element_id_encoding_key),
            unique_claims: args.unique_claims.unwrap_or_default(),
            constrained_properties: args.constrained_properties.unwrap_or_default(),
        },
        seeds,
        args.mutation_id,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(ExecutePlanResult {
        row_count: run.row_count as u64,
        rows_blob: run.rows_blob,
        hot_forward_vertices: run.hot_forward_vertices,
    })
}

pub fn ack_label_stats_deltas_through(through_seq: gleaph_graph_kernel::plan_exec::ShardEventSeq) {
    GraphStore::new().ack_label_stats_deltas_through(through_seq);
}

pub fn list_pending_label_stats_deltas(
    from_seq: gleaph_graph_kernel::plan_exec::ShardEventSeq,
    limit: u32,
) -> Vec<gleaph_graph_kernel::plan_exec::LabelStatsDeltaEventWire> {
    GraphStore::new().pending_label_stats_deltas(from_seq, limit)
}

pub fn get_mutation_journal_entry(
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
) -> Option<gleaph_graph_kernel::plan_exec::GraphMutationJournalEntryWire> {
    GraphStore::new().get_mutation_journal_entry(mutation_id)
}

/// Router → graph (replicated read): the `Acquire` commit proof for each claim. `acquire` is
/// `Some(UniqueAcquireEvidence { effect_id, owner_element_id })` iff a matching pinned `Acquire`
/// effect exists (the `effect_id` lets the Router ack that exact effect after Confirm); `None` is
/// authoritative non-commit while the reservation is non-terminal (ADR 0030 §Timeout). Runs as an
/// update so the answer is replicated — a single-replica query is insufficient evidence to cancel a
/// reservation.
pub fn read_unique_effect_proof(
    claim_ids: Vec<gleaph_graph_kernel::federation::ClaimId>,
) -> Vec<gleaph_graph_kernel::federation::UniqueAcquireProof> {
    let store = GraphStore::new();
    claim_ids
        .into_iter()
        .map(
            |claim_id| gleaph_graph_kernel::federation::UniqueAcquireProof {
                claim_id,
                acquire: store.unique_acquire_evidence(claim_id),
            },
        )
        .collect()
}

/// Router → graph (replicated read): one page of a mutation's pinned `Release` effects (ADR 0030
/// slice 5b), with `effect_ordinal > after_ordinal`, capped at `limit` (the shard also clamps to a
/// hard maximum so an arbitrary-cardinality DELETE cannot overflow the IC response). A `Release` is
/// matched at the Router by `owner_element_id` (not by `ClaimId`, since the releasing mutation
/// differs from the original `Acquire`), so the Router pages through them, reconciles each by owner,
/// and advances the cursor. Runs as an update so the answer is replicated.
pub fn read_unique_release_effects(
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    after_ordinal: Option<u32>,
    limit: u32,
) -> Vec<gleaph_graph_kernel::federation::UniqueEffectReceipt> {
    let limit = (limit as usize).min(MAX_RELEASE_EFFECTS_PAGE);
    GraphStore::new().unique_release_effects_page(mutation_id, after_ordinal, limit)
}

/// Hard cap on a single [`read_unique_release_effects`] page. Each receipt carries an
/// `encoded_value` of at most [`gleaph_gql_ic::unique_key::MAX_UNIQUE_ENCODED_VALUE_LEN`] (2 KiB)
/// plus small fixed fields, so 256 receipts stays well under the IC 2 MiB response limit.
pub const MAX_RELEASE_EFFECTS_PAGE: usize = 256;

/// Router → graph (replicated read): one page of **all** of a mutation's pinned effects (`Acquire`
/// and `Release`), with `effect_ordinal > after_ordinal`, capped at `limit` (clamped to the shard's
/// hard maximum). Backs the Router's unified slice-6 effect recovery (Driver 2): it discovers every
/// un-acked effect — an orphan `Acquire` no reservation can resolve, as well as `Release`s. An empty
/// page is the only end-of-stream signal. Runs as an update so the answer is replicated.
pub fn read_unique_mutation_effects(
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    after_ordinal: Option<u32>,
    limit: u32,
) -> Vec<gleaph_graph_kernel::federation::UniqueEffectReceipt> {
    let limit = (limit as usize).min(MAX_RELEASE_EFFECTS_PAGE);
    GraphStore::new().unique_effects_page(mutation_id, after_ordinal, limit)
}

/// Router → graph: unpin (ack) unique effects after the Router has durably applied them. Per-effect;
/// acking one effect never unpins a sibling of the same mutation (ADR 0030).
pub fn ack_unique_effects(effect_ids: Vec<gleaph_graph_kernel::federation::EffectId>) {
    GraphStore::new().ack_unique_effects(effect_ids);
}

/// Smallest tracked mutation id whose graph-index postings are not yet applied, or
/// `None` when all tracked index work has drained (ADR 0029 Phase 2 watermark). Router
/// uses this to resolve a mutation token's index barrier: a read for mutation `M` is
/// index-satisfied on this shard iff the result is `None` or `M < value`.
pub fn index_pending_min_mutation_id() -> Option<gleaph_graph_kernel::plan_exec::MutationId> {
    GraphStore::new().index_pending_min_mutation_id()
}

#[cfg(feature = "pocket-ic-e2e")]
pub async fn e2e_insert_vertex() -> Result<super::types::E2eInsertVertexResult, String> {
    use crate::index::federation_routing;
    let store = GraphStore::new();
    let vertex_id = store
        .insert_vertex_row(gleaph_graph_kernel::entry::Vertex::default())
        .await
        .map_err(|e| e.to_string())?;
    let global_vertex_id = store
        .global_vertex_id(vertex_id)
        .ok_or_else(|| "global id missing after insert".to_string())?;
    Ok(super::types::E2eInsertVertexResult {
        local_vertex_id: federation_routing::local_vertex_id_raw(vertex_id),
        global_vertex_id,
    })
}

#[cfg(feature = "pocket-ic-e2e")]
pub async fn e2e_insert_vertex_with_property(
    args: super::types::E2eInsertVertexWithPropertyArgs,
) -> Result<super::types::E2eInsertVertexResult, String> {
    use crate::index::pending;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::PropertyId;

    let store = GraphStore::new();
    let vertex_id = store
        .insert_vertex_row(gleaph_graph_kernel::entry::Vertex::default())
        .await
        .map_err(|e| e.to_string())?;
    let property_id = PropertyId::from_raw(args.property_id);
    // E2E scaffolding stands in for the router: supply the indexed catalog for the
    // property under test so DML emits its posting (ADR 0023 D1/D3).
    let _catalog =
        crate::index::catalog_context::enter(gleaph_graph_kernel::index::IndexedPropertyCatalog {
            vertex_property_ids: vec![args.property_id],
            ..Default::default()
        });
    store
        .set_vertex_property(vertex_id, property_id, Value::Int64(args.value))
        .map_err(|e| e.to_string())?;
    let index = wasm_index_client_holder().ok_or("federation not configured")?;
    pending::flush_pending(
        Some(&index as &dyn crate::index::lookup::PropertyIndexLookup),
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    let global_vertex_id = store
        .global_vertex_id(vertex_id)
        .ok_or_else(|| "global id missing after insert".to_string())?;
    Ok(super::types::E2eInsertVertexResult {
        local_vertex_id: crate::index::federation_routing::local_vertex_id_raw(vertex_id),
        global_vertex_id,
    })
}

#[cfg(feature = "pocket-ic-e2e")]
pub async fn e2e_insert_vertex_with_two_properties(
    args: super::types::E2eInsertVertexWithTwoPropertiesArgs,
) -> Result<super::types::E2eInsertVertexResult, String> {
    use crate::index::pending;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::PropertyId;

    let store = GraphStore::new();
    let vertex_id = store
        .insert_vertex_row(gleaph_graph_kernel::entry::Vertex::default())
        .await
        .map_err(|e| e.to_string())?;
    let _catalog =
        crate::index::catalog_context::enter(gleaph_graph_kernel::index::IndexedPropertyCatalog {
            vertex_property_ids: vec![args.property_a, args.property_b],
            ..Default::default()
        });
    store
        .set_vertex_property(
            vertex_id,
            PropertyId::from_raw(args.property_a),
            Value::Int64(args.value_a),
        )
        .map_err(|e| e.to_string())?;
    store
        .set_vertex_property(
            vertex_id,
            PropertyId::from_raw(args.property_b),
            Value::Int64(args.value_b),
        )
        .map_err(|e| e.to_string())?;
    let index = wasm_index_client_holder().ok_or("federation not configured")?;
    pending::flush_pending(
        Some(&index as &dyn crate::index::lookup::PropertyIndexLookup),
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    let global_vertex_id = store
        .global_vertex_id(vertex_id)
        .ok_or_else(|| "global id missing after insert".to_string())?;
    Ok(super::types::E2eInsertVertexResult {
        local_vertex_id: crate::index::federation_routing::local_vertex_id_raw(vertex_id),
        global_vertex_id,
    })
}

#[cfg(feature = "pocket-ic-e2e")]
pub fn e2e_insert_directed_edge(
    args: super::types::E2eInsertDirectedEdgeArgs,
) -> Result<(), String> {
    let store = GraphStore::new();
    let source = ic_stable_lara::VertexId::from(args.source_local_vertex_id);
    let target = ic_stable_lara::VertexId::from(args.target_local_vertex_id);
    store
        .insert_directed_edge(source, target, None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(feature = "pocket-ic-e2e")]
pub async fn e2e_insert_directed_edge_with_property(
    args: super::types::E2eInsertDirectedEdgeWithPropertyArgs,
) -> Result<(), String> {
    use crate::index::edge_pending;
    use crate::index::pending;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId};

    let store = GraphStore::new();
    let source = ic_stable_lara::VertexId::from(args.source_local_vertex_id);
    let target = ic_stable_lara::VertexId::from(args.target_local_vertex_id);
    let label = EdgeLabelId::from_raw(args.edge_label_id);
    let handle = store
        .insert_directed_edge(source, target, Some(label))
        .map_err(|e| e.to_string())?;
    let canonical = store.canonical_edge_handle(handle);
    let property_id = PropertyId::from_raw(args.property_id);
    let _catalog =
        crate::index::catalog_context::enter(gleaph_graph_kernel::index::IndexedPropertyCatalog {
            edge_property_ids: vec![args.property_id],
            ..Default::default()
        });
    store
        .set_edge_property(canonical, property_id, Value::Int64(args.value))
        .map_err(|e| e.to_string())?;
    let index = wasm_index_client_holder().ok_or("federation not configured")?;
    let ix = &index as &dyn crate::index::lookup::PropertyIndexLookup;
    pending::flush_pending(Some(ix), None)
        .await
        .map_err(|e| e.to_string())?;
    edge_pending::flush_pending(Some(ix), None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(feature = "pocket-ic-e2e")]
pub async fn e2e_insert_undirected_edge_with_property(
    args: super::types::E2eInsertUndirectedEdgeWithPropertyArgs,
) -> Result<(), String> {
    use crate::index::edge_pending;
    use crate::index::pending;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId};

    let store = GraphStore::new();
    let source = ic_stable_lara::VertexId::from(args.source_local_vertex_id);
    let target = ic_stable_lara::VertexId::from(args.target_local_vertex_id);
    let label = EdgeLabelId::from_raw(args.edge_label_id);
    let handle = store
        .insert_undirected_edge(source, target, Some(label))
        .map_err(|e| e.to_string())?;
    let canonical = store.canonical_edge_handle(handle);
    let property_id = PropertyId::from_raw(args.property_id);
    let _catalog =
        crate::index::catalog_context::enter(gleaph_graph_kernel::index::IndexedPropertyCatalog {
            edge_property_ids: vec![args.property_id],
            ..Default::default()
        });
    store
        .set_edge_property(canonical, property_id, Value::Int64(args.value))
        .map_err(|e| e.to_string())?;
    let index = wasm_index_client_holder().ok_or("federation not configured")?;
    let ix = &index as &dyn crate::index::lookup::PropertyIndexLookup;
    pending::flush_pending(Some(ix), None)
        .await
        .map_err(|e| e.to_string())?;
    edge_pending::flush_pending(Some(ix), None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Enqueues forward-span compaction for `local_vertex_id` and arms the maintenance
/// timer **without** an inline drain (PocketIC E2E only).
///
/// Production delete/insert paths bound their inline drain with a 32B-instruction
/// budget that fully reclaims at test scale, so the maintenance timer never arms
/// in a PocketIC test through the normal path. This enqueue-only hook leaves a
/// `CompactVertexEdgeSpan` work item in the (stable) deferred queue so a test can
/// exercise the wasm async timer tick (catalog fetch + in-tick posting re-key,
/// ADR 0023 P2) across the upgrade boundary.
#[cfg(feature = "pocket-ic-e2e")]
pub fn e2e_enqueue_forward_compaction(
    args: super::types::E2eEnqueueForwardCompactionArgs,
) -> Result<(), String> {
    use crate::facade::BulkIngestFinalizeSpec;
    use ic_stable_lara::VertexId;

    GraphStore::new()
        .enqueue_bulk_ingest_finalize(&BulkIngestFinalizeSpec {
            forward_vertices: vec![VertexId::from(args.local_vertex_id)],
            reverse_vertices: vec![],
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Deletes the directed edge `source -> target` and flushes the resulting index
/// posting removal (PocketIC E2E only).
///
/// Leaves a tombstone at the deleted edge's slot without compacting the span, so
/// a subsequent [`e2e_enqueue_forward_compaction`] yields a slot-moving
/// compaction. Used in place of GQL `DELETE` because the federated index-served
/// edge `DELETE` plan does not carry the edge element binding to the shard.
#[cfg(feature = "pocket-ic-e2e")]
pub async fn e2e_delete_directed_edge_with_property(
    args: super::types::E2eDeleteDirectedEdgeArgs,
) -> Result<(), String> {
    use crate::facade::EdgeHandle;
    use crate::index::{edge_pending, pending};
    use ic_stable_lara::traits::CsrEdge;
    use ic_stable_lara::{VertexId, labeled::BucketLabelKey as LaraLabelId};

    let store = GraphStore::new();
    let source = VertexId::from(args.source_local_vertex_id);
    let target = VertexId::from(args.target_local_vertex_id);
    let edge = store
        .directed_out_edges(source)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|edge| edge.neighbor_vid() == target)
        .ok_or("directed edge source -> target not found")?;
    let handle = EdgeHandle::at_slot(
        source,
        LaraLabelId::from_raw(edge.label_id),
        edge.edge_slot_index.raw(),
    );
    let _catalog =
        crate::index::catalog_context::enter(gleaph_graph_kernel::index::IndexedPropertyCatalog {
            edge_property_ids: vec![args.property_id],
            ..Default::default()
        });
    store
        .delete_edge_by_handle(handle)
        .map_err(|e| e.to_string())?;
    let index = wasm_index_client_holder().ok_or("federation not configured")?;
    let ix = &index as &dyn crate::index::lookup::PropertyIndexLookup;
    pending::flush_pending(Some(ix), None)
        .await
        .map_err(|e| e.to_string())?;
    edge_pending::flush_pending(Some(ix), None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Pending deferred-maintenance work items in the stable queue (PocketIC E2E only).
///
/// Lets a test poll for timer-driven drain quiescence after advancing PocketIC
/// time to fire the maintenance timer.
#[cfg(feature = "pocket-ic-e2e")]
pub fn e2e_maintenance_queue_len() -> u64 {
    GraphStore::new().maintenance_queue_len()
}

/// Reads the `property_id` value of the directed edge `source -> target` through
/// the **reverse** in-edge → edge-alias → canonical-forward path (PocketIC E2E
/// only). Returns `None` when no such in-edge or property is found.
///
/// This is the alias-resolved read seam for ADR 0023 compaction re-keying: a
/// forward-span compaction moves the canonical forward slot and must re-key both
/// the edge-alias canonical target (`move_canonical_target`) and the property
/// sidecar (`EDGE_PROPERTIES`). Resolving the property from the reverse side
/// (`edge_property` → `canonical_edge_handle_for_sidecar` → alias lookup) reads
/// at the *moved* canonical slot, so a stale alias or un-moved sidecar surfaces
/// here as a missing/wrong value — unlike the forward index-served lookup, which
/// only proves the posting was re-keyed.
#[cfg(feature = "pocket-ic-e2e")]
pub fn e2e_reverse_resolved_edge_property(
    args: super::types::E2eReverseResolvedEdgePropertyArgs,
) -> Result<Option<i64>, String> {
    use crate::facade::EdgeHandle;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::PropertyId;
    use ic_stable_lara::traits::CsrEdge;
    use ic_stable_lara::{VertexId, labeled::BucketLabelKey as LaraLabelId};

    let store = GraphStore::new();
    let source = VertexId::from(args.source_local_vertex_id);
    let target = VertexId::from(args.target_local_vertex_id);
    let property_id = PropertyId::from_raw(args.property_id);

    // Locate the reverse in-edge `target <- source` and rebuild its reverse
    // handle (target row, reverse CSR slot), symmetric to the forward lookup in
    // `e2e_delete_directed_edge_with_property`.
    let Some(in_edge) = store
        .directed_in_edges(target)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|edge| edge.neighbor_vid() == source)
    else {
        return Ok(None);
    };
    let reverse_handle = EdgeHandle::at_slot(
        target,
        LaraLabelId::from_raw(in_edge.label_id),
        in_edge.edge_slot_index.raw(),
    );
    match store.edge_property(reverse_handle, property_id) {
        Some(Value::Int64(v)) => Ok(Some(v)),
        Some(other) => Err(format!("unexpected edge property value: {other:?}")),
        None => Ok(None),
    }
}

pub async fn backfill_label_postings(
    args: gleaph_graph_kernel::federation::PostingBackfillArgs,
) -> Result<gleaph_graph_kernel::federation::PostingBackfillResult, String> {
    let store = GraphStore::new();
    let Some(index) = wasm_index_client_holder() else {
        return Err("federation not configured".into());
    };
    crate::index::label_backfill::backfill_label_postings(&store, &index, args).await
}

pub async fn backfill_vertex_property_postings(
    req: gleaph_graph_kernel::federation::VertexPropertyBackfillRequest,
) -> Result<gleaph_graph_kernel::federation::PostingBackfillResult, String> {
    let store = GraphStore::new();
    let Some(index) = wasm_index_client_holder() else {
        return Err("federation not configured".into());
    };
    // ADR 0023 D1/D5: the router supplies the indexed catalog per backfill step.
    let _catalog = crate::index::catalog_context::enter(req.catalog);
    crate::index::vertex_property_backfill::backfill_vertex_property_postings(
        &store, &index, req.args,
    )
    .await
}

pub async fn backfill_edge_property_postings(
    req: gleaph_graph_kernel::federation::EdgePropertyBackfillRequest,
) -> Result<gleaph_graph_kernel::federation::EdgePostingBackfillResult, String> {
    let store = GraphStore::new();
    let Some(index) = wasm_index_client_holder() else {
        return Err("federation not configured".into());
    };
    let _catalog = crate::index::catalog_context::enter(req.catalog);
    crate::index::edge_property_backfill::backfill_edge_property_postings(&store, &index, req.args)
        .await
}

/// Maximum vertices per finalize call (`forward` + `reverse` lists).
const MAX_BULK_INGEST_FINALIZE_VERTICES: usize = 256;

pub fn finalize_bulk_ingest(
    args: gleaph_graph_kernel::federation::BulkIngestFinalizeArgs,
) -> Result<gleaph_graph_kernel::federation::BulkIngestFinalizeResult, String> {
    use crate::facade::{BulkIngestFinalizeReport, BulkIngestFinalizeSpec};
    use ic_stable_lara::VertexId;

    let store = GraphStore::new();
    let routing = store
        .federation_routing()
        .ok_or("federation routing not configured")?;
    if routing.shard_id != args.target_shard_id {
        return Err(format!(
            "target_shard_id {} does not match this graph shard {}",
            args.target_shard_id, routing.shard_id
        ));
    }
    let total_vertices = args.forward_vertices.len() + args.reverse_vertices.len();
    if total_vertices > MAX_BULK_INGEST_FINALIZE_VERTICES {
        return Err(format!(
            "vertex list too long: {total_vertices} > {MAX_BULK_INGEST_FINALIZE_VERTICES}"
        ));
    }

    let spec = BulkIngestFinalizeSpec {
        forward_vertices: args
            .forward_vertices
            .iter()
            .copied()
            .map(VertexId::from)
            .collect(),
        reverse_vertices: args
            .reverse_vertices
            .iter()
            .copied()
            .map(VertexId::from)
            .collect(),
    };

    let report = if args.enqueue {
        store
            .finalize_bulk_ingest(&spec)
            .map_err(|e| e.to_string())?
    } else {
        let maintenance = store
            .run_bulk_ingest_finalize_drain()
            .map_err(|e| e.to_string())?;
        BulkIngestFinalizeReport {
            maintenance,
            queued_forward: 0,
            queued_reverse: 0,
        }
    };

    Ok(gleaph_graph_kernel::federation::BulkIngestFinalizeResult {
        queued_forward: report.queued_forward,
        queued_reverse: report.queued_reverse,
        processed_work_items: report.maintenance.work.processed_work_items,
        remaining_queue_len: report.maintenance.remaining_queue_len(),
        instruction_budget_exhausted: report.maintenance.instruction_budget_exhausted,
        instructions_used: report.maintenance.instructions_used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Encode, Principal};
    use gleaph_gql::Value;
    use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
    use gleaph_gql::types::LabelExpr;
    use gleaph_gql_ic::encode_gql_params_blob;
    use gleaph_gql_planner::plan::ScanValue;
    use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn};
    use gleaph_gql_planner::wire::encode_block_plans;
    use gleaph_graph_kernel::federation::{BulkIngestFinalizeArgs, ElementIdEncodingKey, ShardId};
    use gleaph_graph_kernel::plan_exec::{SeedBindingEntry, SeedBindingsWire};

    const TEST_SHARD_ID: ShardId = ShardId::new(0);
    const TEST_ELEMENT_ID_ENCODING_KEY: [u8; 16] = ElementIdEncodingKey::host_test_fixture().0;

    fn attach_test_federation(shard_id: ShardId) {
        let mut metadata = GraphMetadata::default();
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            shard_id,
        }));
        GraphStore::new()
            .set_metadata(metadata)
            .expect("attach federation routing");
    }

    fn label_intersection_plan_with_seeds(
        store: &GraphStore,
        person_label: &str,
        employee_label: &str,
    ) -> (Vec<u8>, Vec<u8>, u32) {
        let vid = store
            .insert_vertex_named([person_label, employee_label], Vec::<(&str, Value)>::new())
            .expect("vertex with both labels");
        let _person_only = store
            .insert_vertex_named([person_label], Vec::<(&str, Value)>::new())
            .expect("person only");
        let local_vid = u32::try_from(u64::from(vid)).expect("local vertex id");
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some(person_label.into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::IsLabeled {
                    expr: Box::new(Expr::new(ExprKind::Variable("n".into()))),
                    label: LabelExpr::Name(employee_label.into()),
                    negated: false,
                })],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".into())),
                    alias: Some("n".into()),
                }],
                distinct: false,
            },
        ]);
        let plan_blob = encode_block_plans(&[plan], false).expect("encode plan");
        let seeds = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "n".into(),
                local_vertex_ids: vec![local_vid],
                local_edge_postings: Vec::new(),
            }],
        };
        let seed_blob = Encode!(&seeds).expect("encode seeds");
        (plan_blob, seed_blob, local_vid)
    }

    #[test]
    fn execute_plan_query_seed_bindings_skip_label_intersection() {
        attach_test_federation(TEST_SHARD_ID);
        let store = GraphStore::new();
        let (plan_blob, seed_blob, _local_vid) =
            label_intersection_plan_with_seeds(&store, "HandlerSeedPerson", "HandlerSeedEmployee");
        let params_blob = encode_gql_params_blob(vec![]).expect("encode params");
        let args = ExecutePlanArgs {
            target_shard_id: TEST_SHARD_ID,
            element_id_encoding_key: TEST_ELEMENT_ID_ENCODING_KEY,
            mutation_id: None,
            plan_blob,
            params_blob,
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
        };

        let result = pollster::block_on(execute_plan_query(args)).expect("execute_plan_query");

        assert_eq!(result.row_count, 1);
        let rows_blob = result.rows_blob.expect("query rows_blob");
        let wire = gleaph_gql_ic::IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode");
        let materialized =
            crate::plan::plan_query_result_from_ic_wire(wire).expect("materialize rows");
        assert_eq!(materialized.rows.len(), 1);
        assert!(materialized.rows[0].contains_key("n"));
    }

    #[test]
    fn execute_plan_query_seed_bindings_skip_equality_index_scan() {
        attach_test_federation(TEST_SHARD_ID);
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["HandlerIxSeedEq"], [("age", Value::Uint8(5))])
            .expect("vertex");
        let local_vid = u32::try_from(u64::from(vid)).expect("local vertex id");
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "age".into(),
                value: ScanValue::Literal(Value::Int64(5)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".into())),
                    alias: Some("n".into()),
                }],
                distinct: false,
            },
        ]);
        let plan_blob = encode_block_plans(&[plan], false).expect("encode plan");
        let seeds = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "n".into(),
                local_vertex_ids: vec![local_vid],
                local_edge_postings: Vec::new(),
            }],
        };
        let seed_blob = Encode!(&seeds).expect("encode seeds");
        let params_blob = encode_gql_params_blob(vec![]).expect("encode params");
        let args = ExecutePlanArgs {
            target_shard_id: TEST_SHARD_ID,
            element_id_encoding_key: TEST_ELEMENT_ID_ENCODING_KEY,
            mutation_id: None,
            plan_blob,
            params_blob,
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
        };

        let result = pollster::block_on(execute_plan_query(args)).expect("execute_plan_query");

        assert_eq!(result.row_count, 1);
    }

    #[test]
    fn execute_plan_query_federated_rejects_index_scan_without_seeds() {
        attach_test_federation(TEST_SHARD_ID);
        let store = GraphStore::new();
        let _ = store
            .insert_vertex_named(["HandlerNoSeedIx"], [("age", Value::Uint8(5))])
            .expect("vertex");
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "age".into(),
                value: ScanValue::Literal(Value::Int64(5)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".into())),
                    alias: Some("n".into()),
                }],
                distinct: false,
            },
        ]);
        let plan_blob = encode_block_plans(&[plan], false).expect("encode plan");
        let params_blob = encode_gql_params_blob(vec![]).expect("encode params");
        let args = ExecutePlanArgs {
            target_shard_id: TEST_SHARD_ID,
            element_id_encoding_key: TEST_ELEMENT_ID_ENCODING_KEY,
            mutation_id: None,
            plan_blob,
            params_blob,
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: None,
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
        };

        let err = pollster::block_on(execute_plan_query(args)).expect_err("missing seeds");
        assert!(
            err.contains("IndexScan(no index client)"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn execute_plan_query_rejects_shard_mismatch() {
        attach_test_federation(TEST_SHARD_ID);
        let store = GraphStore::new();
        let (plan_blob, seed_blob, _) = label_intersection_plan_with_seeds(
            &store,
            "HandlerShardPerson",
            "HandlerShardEmployee",
        );
        let params_blob = encode_gql_params_blob(vec![]).expect("encode params");
        let args = ExecutePlanArgs {
            target_shard_id: ShardId::new(1),
            element_id_encoding_key: TEST_ELEMENT_ID_ENCODING_KEY,
            mutation_id: None,
            plan_blob,
            params_blob,
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
        };

        let err = pollster::block_on(execute_plan_query(args)).expect_err("shard mismatch");
        assert!(
            err.contains("target_shard_id") && err.contains("does not match"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn finalize_bulk_ingest_rejects_shard_mismatch() {
        attach_test_federation(TEST_SHARD_ID);
        let args = BulkIngestFinalizeArgs {
            target_shard_id: ShardId::new(1),
            forward_vertices: vec![1],
            reverse_vertices: vec![],
            enqueue: true,
        };
        let err = finalize_bulk_ingest(args).expect_err("shard mismatch");
        assert!(
            err.contains("target_shard_id") && err.contains("does not match"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn finalize_bulk_ingest_rejects_vertex_list_over_limit() {
        attach_test_federation(TEST_SHARD_ID);
        let args = BulkIngestFinalizeArgs {
            target_shard_id: TEST_SHARD_ID,
            forward_vertices: vec![0; 200],
            reverse_vertices: vec![0; 57],
            enqueue: true,
        };
        let err = finalize_bulk_ingest(args).expect_err("vertex limit");
        assert!(
            err.contains("vertex list too long"),
            "unexpected error: {err}"
        );
    }
}
