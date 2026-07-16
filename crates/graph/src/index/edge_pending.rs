//! Pending federated edge property index postings (ADR 0009 §1).

use crate::facade::{GraphStore, RepairPostingOp};
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use crate::property::PropertyIndexOp;
use gleaph_graph_kernel::index::IndexPostingMutation;
use ic_stable_lara::VertexId;
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub(crate) enum PendingEdgePostingOp {
    Insert {
        property_id: u32,
        payload_bytes: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    },
    Remove {
        property_id: u32,
        payload_bytes: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    },
}

thread_local! {
    static PENDING: RefCell<Vec<PendingEdgePostingOp>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn clear_pending() {
    PENDING.with(|p| p.borrow_mut().clear());
}

fn push(op: PendingEdgePostingOp) {
    if !GraphStore::new().federation_configured() {
        return;
    }
    PENDING.with(|p| p.borrow_mut().push(op));
}

pub(crate) fn take_pending() -> Vec<PendingEdgePostingOp> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

pub(crate) fn to_repair_op(op: &PendingEdgePostingOp) -> RepairPostingOp {
    let (remove, property_id, payload_bytes, label_id, owner_vertex_id, slot_index) = match op {
        PendingEdgePostingOp::Insert {
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
        } => (
            false,
            *property_id,
            payload_bytes.clone(),
            *label_id,
            *owner_vertex_id,
            *slot_index,
        ),
        PendingEdgePostingOp::Remove {
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
        } => (
            true,
            *property_id,
            payload_bytes.clone(),
            *label_id,
            *owner_vertex_id,
            *slot_index,
        ),
    };
    RepairPostingOp::EdgeProperty {
        remove,
        property_id,
        payload_bytes,
        label_id,
        owner_vertex_id,
        slot_index,
    }
}

pub(crate) fn to_index_mutation(op: &PendingEdgePostingOp) -> IndexPostingMutation {
    match op {
        PendingEdgePostingOp::Insert {
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
        } => IndexPostingMutation::EdgeProperty {
            remove: false,
            property_id: *property_id,
            value: payload_bytes.clone(),
            label_id: *label_id,
            owner_vertex_id: *owner_vertex_id,
            slot_index: *slot_index,
        },
        PendingEdgePostingOp::Remove {
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
        } => IndexPostingMutation::EdgeProperty {
            remove: true,
            property_id: *property_id,
            value: payload_bytes.clone(),
            label_id: *label_id,
            owner_vertex_id: *owner_vertex_id,
            slot_index: *slot_index,
        },
    }
}

/// Queue removals for every indexed property on an edge being deleted (federated index sync).
pub(crate) fn enqueue_removals_for_edge(owner_vertex_id: VertexId, label_id: u16, slot_index: u32) {
    let owner_raw = u32::try_from(u64::from(owner_vertex_id)).unwrap_or(0);
    GraphStore::for_each_indexed_edge_property_on_edge(
        owner_vertex_id,
        label_id,
        slot_index,
        |pid, payload_bytes| {
            push(PendingEdgePostingOp::Remove {
                property_id: pid.raw(),
                payload_bytes,
                label_id,
                owner_vertex_id: owner_raw,
                slot_index,
            });
        },
    );
}

pub(crate) fn push_edge_index_op(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
    op: PropertyIndexOp,
) {
    let owner_raw = u32::try_from(u64::from(owner_vertex_id)).unwrap_or(0);
    let pending = match op {
        PropertyIndexOp::Insert {
            property_id,
            payload_bytes,
        } => PendingEdgePostingOp::Insert {
            property_id: property_id.raw(),
            payload_bytes,
            label_id,
            owner_vertex_id: owner_raw,
            slot_index,
        },
        PropertyIndexOp::Remove {
            property_id,
            payload_bytes,
        } => PendingEdgePostingOp::Remove {
            property_id: property_id.raw(),
            payload_bytes,
            label_id,
            owner_vertex_id: owner_raw,
            slot_index,
        },
    };
    push(pending);
}

async fn compensate_index_ops(
    ix: &dyn PropertyIndexLookup,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    applied: &[PendingEdgePostingOp],
) -> Result<(), PlanQueryError> {
    for op in applied.iter().rev() {
        match op {
            PendingEdgePostingOp::Insert {
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
            } => {
                ix.edge_posting_remove_at(
                    shard_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await?;
            }
            PendingEdgePostingOp::Remove {
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
            } => {
                ix.edge_posting_insert_at(
                    shard_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn flush_pending(
    index: Option<&dyn PropertyIndexLookup>,
    mutation_id: Option<u64>,
) -> Result<(), PlanQueryError> {
    let mutation_id = mutation_id.unwrap_or(0);
    if !GraphStore::new().federation_configured() {
        clear_pending();
        return Ok(());
    }
    let Some(ix) = index else {
        clear_pending();
        return Err(PlanQueryError::UnsupportedOp(
            "edge index mutations dropped (no index client)",
        ));
    };
    let ops: Vec<PendingEdgePostingOp> = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    if ops.is_empty() {
        return Ok(());
    }
    let shard_id = ix.local_shard_id();
    if ix.supports_posting_batch() {
        let mutations: Vec<IndexPostingMutation> = ops.iter().map(to_index_mutation).collect();
        let mut offset = 0usize;
        while offset < ops.len() {
            let chunk_end = offset
                + crate::index::batch::posting_batch_chunk_end(shard_id, &mutations[offset..], 0);
            let operations = mutations[offset..chunk_end].to_vec();
            let progress = match ix.posting_batch_at(shard_id, operations).await {
                Ok(progress) => progress,
                Err(error) => {
                    GraphStore::new()
                        .repair_journal_append(mutation_id, ops[offset..].iter().map(to_repair_op));
                    crate::facade::maintenance_timer::arm_if_needed();
                    return Err(PlanQueryError::IndexFlushDeferred {
                        op: "edge_batch",
                        detail: error.to_string(),
                    });
                }
            };
            let advanced = usize::try_from(progress.applied).unwrap_or(0);
            if advanced == 0 || advanced > ops.len().saturating_sub(offset) {
                GraphStore::new()
                    .repair_journal_append(mutation_id, ops[offset..].iter().map(to_repair_op));
                crate::facade::maintenance_timer::arm_if_needed();
                return Err(PlanQueryError::IndexFlushDeferred {
                    op: "edge_batch_budget",
                    detail: "index batch made no progress".into(),
                });
            }
            offset = offset.saturating_add(advanced);
            if progress.next_index.is_none() {
                if offset == ops.len() {
                    return Ok(());
                }
                if offset == chunk_end {
                    continue;
                }
                GraphStore::new()
                    .repair_journal_append(mutation_id, ops[offset..].iter().map(to_repair_op));
                crate::facade::maintenance_timer::arm_if_needed();
                return Err(PlanQueryError::IndexFlushDeferred {
                    op: "edge_batch_progress",
                    detail: "index batch returned an invalid terminal progress".into(),
                });
            }
        }
        return Ok(());
    }

    let mut applied: Vec<PendingEdgePostingOp> = Vec::with_capacity(ops.len());
    for op in &ops {
        let result = match op {
            PendingEdgePostingOp::Insert {
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
            } => {
                ix.edge_posting_insert_at(
                    shard_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await
            }
            PendingEdgePostingOp::Remove {
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
            } => {
                ix.edge_posting_remove_at(
                    shard_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await
            }
        };

        if let Err(primary) = result {
            match compensate_index_ops(ix, shard_id, &applied).await {
                Ok(()) => {
                    // Index is back at its pre-batch state; persist the whole
                    // batch durably (ADR 0023 D5) and arm the timer to re-apply.
                    // The batch is durable and the index converges async (ADR 0024).
                    GraphStore::new()
                        .repair_journal_append(mutation_id, ops.iter().map(to_repair_op));
                    crate::facade::maintenance_timer::arm_if_needed();
                    return Err(PlanQueryError::IndexFlushDeferred {
                        op: "edge_flush",
                        detail: primary.to_string(),
                    });
                }
                Err(rollback_err) => {
                    // Compensation failed: do not trap (ADR 0023 P4). Persist the
                    // full batch so idempotent re-application converges the index
                    // to the store (ADR 0024), then surface the deferred error.
                    GraphStore::new()
                        .repair_journal_append(mutation_id, ops.iter().map(to_repair_op));
                    crate::facade::maintenance_timer::arm_if_needed();
                    return Err(PlanQueryError::IndexFlushDeferred {
                        op: "edge_compensate",
                        detail: format!(
                            "primary: {primary}; rollback: {rollback_err}; batch journaled for repair"
                        ),
                    });
                }
            }
        }
        applied.push(op.clone());
    }
    Ok(())
}
