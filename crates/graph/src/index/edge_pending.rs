//! Pending federated edge property index postings (ADR 0009 §1).

use crate::facade::{GraphStore, RepairPostingOp};
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use crate::property::PropertyIndexOp;
use gleaph_graph_kernel::entry::VertexId;
use gleaph_graph_kernel::index::{IndexMaintenancePhase, IndexPostingMutation, PhysicalIndexId};
use std::cell::RefCell;
use std::ops::Range;

#[derive(Clone, Debug)]
pub(crate) enum PendingEdgePostingOp {
    Insert {
        physical_index_id: PhysicalIndexId,
        catalog_epoch: u64,
        phase: IndexMaintenancePhase,
        property_id: u32,
        payload_bytes: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    },
    Remove {
        physical_index_id: PhysicalIndexId,
        catalog_epoch: u64,
        phase: IndexMaintenancePhase,
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
    let (
        remove,
        physical_index_id,
        catalog_epoch,
        phase,
        property_id,
        payload_bytes,
        label_id,
        owner_vertex_id,
        slot_index,
    ) = match op {
        PendingEdgePostingOp::Insert {
            physical_index_id,
            catalog_epoch,
            phase,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
        } => (
            false,
            *physical_index_id,
            *catalog_epoch,
            *phase,
            *property_id,
            payload_bytes.clone(),
            *label_id,
            *owner_vertex_id,
            *slot_index,
        ),
        PendingEdgePostingOp::Remove {
            physical_index_id,
            catalog_epoch,
            phase,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
        } => (
            true,
            *physical_index_id,
            *catalog_epoch,
            *phase,
            *property_id,
            payload_bytes.clone(),
            *label_id,
            *owner_vertex_id,
            *slot_index,
        ),
    };
    RepairPostingOp::EdgeProperty {
        physical_index_id,
        catalog_epoch,
        phase,
        remove,
        property_id,
        payload_bytes,
        label_id,
        owner_vertex_id,
        slot_index,
    }
}

pub(crate) fn to_index_mutation(
    op: &PendingEdgePostingOp,
) -> Result<IndexPostingMutation, PlanQueryError> {
    match op {
        PendingEdgePostingOp::Insert {
            physical_index_id,
            phase,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
            ..
        } if phase.is_active() => Ok(IndexPostingMutation::EdgeProperty {
            physical_index_id: *physical_index_id,
            remove: false,
            property_id: (*property_id).into(),
            value: payload_bytes.clone(),
            label_id: (*label_id).into(),
            owner_vertex_id: (*owner_vertex_id).into(),
            slot_index: *slot_index,
        }),
        PendingEdgePostingOp::Remove {
            physical_index_id,
            phase,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
            ..
        } if phase.is_active() => Ok(IndexPostingMutation::EdgeProperty {
            physical_index_id: *physical_index_id,
            remove: true,
            property_id: (*property_id).into(),
            value: payload_bytes.clone(),
            label_id: (*label_id).into(),
            owner_vertex_id: (*owner_vertex_id).into(),
            slot_index: *slot_index,
        }),
        PendingEdgePostingOp::Insert { .. } | PendingEdgePostingOp::Remove { .. } => {
            Err(PlanQueryError::UnsupportedOp(
                "index posting dispatch requires Active maintenance phase",
            ))
        }
    }
}

/// The mutations that carry `ops`, in order, each with the ops it covers. A removal followed by
/// the insert of the same posting at another slot of the same owner and label, the two ops an
/// edge's move under compaction queues, is one [`IndexPostingMutation::EdgePropertyMove`], so
/// its value is sent once; every other op is a mutation of its own.
pub(crate) fn to_index_mutations(
    ops: &[PendingEdgePostingOp],
) -> Result<Vec<(IndexPostingMutation, Range<usize>)>, PlanQueryError> {
    let mut out = Vec::with_capacity(ops.len());
    let mut at = 0;
    while let Some(op) = ops.get(at) {
        if let Some(moved) = ops.get(at + 1).and_then(|next| as_move(op, next)) {
            out.push((moved, at..at + 2));
            at += 2;
        } else {
            out.push((to_index_mutation(op)?, at..at + 1));
            at += 1;
        }
    }
    Ok(out)
}

/// `removed` then `inserted` as one move, when they are the same Active posting at two slots.
fn as_move(
    removed: &PendingEdgePostingOp,
    inserted: &PendingEdgePostingOp,
) -> Option<IndexPostingMutation> {
    let (
        PendingEdgePostingOp::Remove {
            physical_index_id,
            catalog_epoch,
            phase,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index: from_slot,
        },
        PendingEdgePostingOp::Insert {
            physical_index_id: to_index,
            catalog_epoch: to_epoch,
            phase: to_phase,
            property_id: to_property,
            payload_bytes: to_bytes,
            label_id: to_label,
            owner_vertex_id: to_owner,
            slot_index: to_slot,
        },
    ) = (removed, inserted)
    else {
        return None;
    };
    let same = (
        physical_index_id,
        catalog_epoch,
        phase,
        property_id,
        payload_bytes,
        label_id,
        owner_vertex_id,
    ) == (
        to_index,
        to_epoch,
        to_phase,
        to_property,
        to_bytes,
        to_label,
        to_owner,
    );
    (same && phase.is_active() && from_slot != to_slot).then(|| {
        IndexPostingMutation::EdgePropertyMove {
            physical_index_id: *physical_index_id,
            property_id: (*property_id).into(),
            value: payload_bytes.clone(),
            label_id: (*label_id).into(),
            owner_vertex_id: (*owner_vertex_id).into(),
            from_slot: *from_slot,
            to_slot: *to_slot,
        }
    })
}

/// One Active edge posting moving with its edge under compaction: removed at `from_slot` and
/// inserted at `to_slot`, with one encoded value.
pub(crate) struct PostingMove {
    pub(crate) owner_vertex_id: VertexId,
    pub(crate) label_id: u16,
    pub(crate) from_slot: u32,
    pub(crate) to_slot: u32,
    pub(crate) membership: crate::index::catalog_context::IndexMembershipRef,
    pub(crate) property_id: u32,
    pub(crate) payload_bytes: Vec<u8>,
}

/// Queues each of a compaction step's posting moves as its removal followed by its insert, so
/// that the flush sends the pair as one move ([`to_index_mutations`]); the federation is checked
/// once for them all.
pub(crate) fn push_moves(moves: Vec<PostingMove>) {
    if moves.is_empty() || !GraphStore::new().federation_configured() {
        return;
    }
    PENDING.with_borrow_mut(|pending| {
        pending.reserve(moves.len() * 2);
        for moved in moves {
            let owner_vertex_id = u32::try_from(u64::from(moved.owner_vertex_id)).unwrap_or(0);
            let membership = moved.membership;
            pending.push(PendingEdgePostingOp::Remove {
                physical_index_id: membership.physical_index_id,
                catalog_epoch: membership.catalog_epoch,
                phase: membership.phase,
                property_id: moved.property_id,
                payload_bytes: moved.payload_bytes.clone(),
                label_id: moved.label_id,
                owner_vertex_id,
                slot_index: moved.from_slot,
            });
            pending.push(PendingEdgePostingOp::Insert {
                physical_index_id: membership.physical_index_id,
                catalog_epoch: membership.catalog_epoch,
                phase: membership.phase,
                property_id: moved.property_id,
                payload_bytes: moved.payload_bytes,
                label_id: moved.label_id,
                owner_vertex_id,
                slot_index: moved.to_slot,
            });
        }
    });
}

/// Queue removals for every indexed property on an edge being deleted (federated index sync).
///
/// Building/Sealing memberships are intentionally excluded: their removals are admitted through
/// the index-build fence (Memory46 outbox) before the canonical delete, never through the
/// ordinary Active-only queue.
pub(crate) fn enqueue_removals_for_edge(owner_vertex_id: VertexId, label_id: u16, slot_index: u32) {
    let owner_raw = u32::try_from(u64::from(owner_vertex_id)).unwrap_or(0);
    GraphStore::for_each_indexed_edge_property_on_edge(
        owner_vertex_id,
        label_id,
        slot_index,
        |membership, pid, payload_bytes| {
            if !membership.phase.is_active() {
                return;
            }
            push(PendingEdgePostingOp::Remove {
                physical_index_id: membership.physical_index_id,
                catalog_epoch: membership.catalog_epoch,
                phase: membership.phase,
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
    membership: crate::index::catalog_context::IndexMembershipRef,
    op: PropertyIndexOp,
) {
    let owner_raw = u32::try_from(u64::from(owner_vertex_id)).unwrap_or(0);
    let pending = match op {
        PropertyIndexOp::Insert {
            property_id,
            payload_bytes,
        } => PendingEdgePostingOp::Insert {
            physical_index_id: membership.physical_index_id,
            catalog_epoch: membership.catalog_epoch,
            phase: membership.phase,
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
            physical_index_id: membership.physical_index_id,
            catalog_epoch: membership.catalog_epoch,
            phase: membership.phase,
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
                physical_index_id,
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
                ..
            } => {
                ix.edge_posting_remove_at(
                    shard_id,
                    *physical_index_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await?;
            }
            PendingEdgePostingOp::Remove {
                physical_index_id,
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
                ..
            } => {
                ix.edge_posting_insert_at(
                    shard_id,
                    *physical_index_id,
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
    let ops: Vec<PendingEdgePostingOp> = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    if ops.is_empty() {
        return Ok(());
    }
    let Some(ix) = index else {
        GraphStore::new().repair_journal_append(mutation_id, ops.iter().map(to_repair_op));
        crate::facade::maintenance_timer::arm_if_needed();
        return Err(PlanQueryError::IndexFlushDeferred {
            op: "edge_no_index_client",
            detail: "index client unavailable; posting batch journaled for repair".into(),
        });
    };
    if ops.iter().any(|op| {
        matches!(
            op,
            PendingEdgePostingOp::Insert { phase, .. }
                | PendingEdgePostingOp::Remove { phase, .. }
                if !phase.is_active()
        )
    }) {
        GraphStore::new().repair_journal_append(mutation_id, ops.iter().map(to_repair_op));
        crate::facade::maintenance_timer::arm_if_needed();
        return Err(PlanQueryError::UnsupportedOp(
            "index posting dispatch requires Active maintenance phase",
        ));
    }
    let shard_id = ix.local_shard_id();
    if ix.supports_posting_batch() {
        let carried = to_index_mutations(&ops)?;
        let mutations: Vec<IndexPostingMutation> = carried
            .iter()
            .map(|(mutation, _)| mutation.clone())
            .collect();
        // The first op the mutation at `offset` carries; past the end, every op is sent.
        let first_op = |offset: usize| carried.get(offset).map_or(ops.len(), |(_, ops)| ops.start);
        let mut offset = 0usize;
        while offset < mutations.len() {
            let chunk_end = offset
                + crate::index::batch::posting_batch_chunk_end(shard_id, &mutations[offset..], 0);
            let operations = mutations[offset..chunk_end].to_vec();
            let progress = match ix.posting_batch_at(shard_id, operations).await {
                Ok(progress) => progress,
                Err(error) => {
                    GraphStore::new().repair_journal_append(
                        mutation_id,
                        ops[first_op(offset)..].iter().map(to_repair_op),
                    );
                    crate::facade::maintenance_timer::arm_if_needed();
                    return Err(PlanQueryError::IndexFlushDeferred {
                        op: "edge_batch",
                        detail: error.to_string(),
                    });
                }
            };
            let advanced = usize::try_from(progress.applied).unwrap_or(0);
            if advanced == 0 || advanced > chunk_end.saturating_sub(offset) {
                GraphStore::new().repair_journal_append(
                    mutation_id,
                    ops[first_op(offset)..].iter().map(to_repair_op),
                );
                crate::facade::maintenance_timer::arm_if_needed();
                return Err(PlanQueryError::IndexFlushDeferred {
                    op: "edge_batch_budget",
                    detail: "index batch made no progress".into(),
                });
            }
            offset = offset.saturating_add(advanced);
            if progress.next_index.is_none() {
                if offset == mutations.len() {
                    return Ok(());
                }
                if offset == chunk_end {
                    continue;
                }
                GraphStore::new().repair_journal_append(
                    mutation_id,
                    ops[first_op(offset)..].iter().map(to_repair_op),
                );
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
                physical_index_id,
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
                ..
            } => {
                ix.edge_posting_insert_at(
                    shard_id,
                    *physical_index_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await
            }
            PendingEdgePostingOp::Remove {
                physical_index_id,
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_id,
                slot_index,
                ..
            } => {
                ix.edge_posting_remove_at(
                    shard_id,
                    *physical_index_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: PhysicalIndexId = PhysicalIndexId::new(900_201).expect("test physical id");

    fn remove(slot_index: u32, payload: &[u8]) -> PendingEdgePostingOp {
        PendingEdgePostingOp::Remove {
            physical_index_id: INDEX,
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            property_id: 7,
            payload_bytes: payload.to_vec(),
            label_id: 3,
            owner_vertex_id: 10,
            slot_index,
        }
    }

    fn insert(slot_index: u32, payload: &[u8]) -> PendingEdgePostingOp {
        PendingEdgePostingOp::Insert {
            physical_index_id: INDEX,
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            property_id: 7,
            payload_bytes: payload.to_vec(),
            label_id: 3,
            owner_vertex_id: 10,
            slot_index,
        }
    }

    fn shapes(ops: &[PendingEdgePostingOp]) -> Vec<(&'static str, core::ops::Range<usize>)> {
        to_index_mutations(ops)
            .expect("Active ops")
            .into_iter()
            .map(|(mutation, covered)| {
                let shape = match mutation {
                    IndexPostingMutation::EdgePropertyMove { .. } => "move",
                    IndexPostingMutation::EdgeProperty { remove: true, .. } => "remove",
                    IndexPostingMutation::EdgeProperty { remove: false, .. } => "insert",
                    _ => "other",
                };
                (shape, covered)
            })
            .collect()
    }

    #[test]
    fn a_removal_then_the_same_posting_at_another_slot_is_one_move() {
        let ops = [
            remove(5, b"v"),
            insert(2, b"v"),
            remove(9, b"w"),
            insert(4, b"w"),
        ];
        assert_eq!(shapes(&ops), [("move", 0..2), ("move", 2..4)]);
        let Ok(carried) = to_index_mutations(&ops) else {
            panic!("Active ops")
        };
        assert!(matches!(
            &carried[0].0,
            IndexPostingMutation::EdgePropertyMove { from_slot: 5, to_slot: 2, value, .. }
                if value == b"v"
        ));
    }

    #[test]
    fn other_ops_stay_apart() {
        // A different value, an insert before the removal, and the same slot are no moves.
        let ops = [
            remove(5, b"v"),
            insert(2, b"w"),
            insert(1, b"x"),
            remove(6, b"x"),
            remove(3, b"y"),
            insert(3, b"y"),
        ];
        assert_eq!(
            shapes(&ops),
            [
                ("remove", 0..1),
                ("insert", 1..2),
                ("insert", 2..3),
                ("remove", 3..4),
                ("remove", 4..5),
                ("insert", 5..6),
            ]
        );
    }
}
