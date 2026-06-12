//! Record vertex label membership changes for the federated label index.

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::entry::VertexLabelId;
use ic_stable_lara::VertexId;
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub(crate) enum PendingLabelOp {
    Insert { label_id: u32, vertex_id: u32 },
    Remove { label_id: u32, vertex_id: u32 },
}

thread_local! {
    static PENDING: RefCell<Vec<PendingLabelOp>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn clear_pending() {
    PENDING.with(|p| p.borrow_mut().clear());
}

fn push(op: PendingLabelOp) {
    if !GraphStore::new().federation_configured() {
        return;
    }
    PENDING.with(|p| p.borrow_mut().push(op));
}

pub(crate) fn record_vertex_label_set(
    vertex_id: VertexId,
    prev: &[VertexLabelId],
    next: &[VertexLabelId],
) {
    let vid = u32::try_from(u64::from(vertex_id)).unwrap_or(0);
    for label in prev {
        if !next.contains(label) {
            push(PendingLabelOp::Remove {
                label_id: u32::from(label.raw()),
                vertex_id: vid,
            });
        }
    }
    for label in next {
        if !prev.contains(label) {
            push(PendingLabelOp::Insert {
                label_id: u32::from(label.raw()),
                vertex_id: vid,
            });
        }
    }
}

async fn compensate_label_ops(
    ix: &dyn PropertyIndexLookup,
    applied: &[PendingLabelOp],
) -> Result<(), PlanQueryError> {
    for op in applied.iter().rev() {
        match op {
            PendingLabelOp::Insert {
                label_id,
                vertex_id,
            } => {
                ix.label_posting_remove(*label_id, *vertex_id).await?;
            }
            PendingLabelOp::Remove {
                label_id,
                vertex_id,
            } => {
                ix.label_posting_insert(*label_id, *vertex_id).await?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn flush_pending(
    index: Option<&dyn PropertyIndexLookup>,
) -> Result<(), PlanQueryError> {
    if !GraphStore::new().federation_configured() {
        clear_pending();
        return Ok(());
    }
    let Some(ix) = index else {
        clear_pending();
        return Err(PlanQueryError::UnsupportedOp(
            "label index mutations dropped (no index client)",
        ));
    };
    let ops: Vec<PendingLabelOp> = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    if ops.is_empty() {
        return Ok(());
    }

    let mut applied: Vec<PendingLabelOp> = Vec::with_capacity(ops.len());
    for op in &ops {
        let result = match op {
            PendingLabelOp::Insert {
                label_id,
                vertex_id,
            } => ix.label_posting_insert(*label_id, *vertex_id).await,
            PendingLabelOp::Remove {
                label_id,
                vertex_id,
            } => ix.label_posting_remove(*label_id, *vertex_id).await,
        };

        if let Err(primary) = result {
            match compensate_label_ops(ix, &applied).await {
                Ok(()) => {
                    PENDING.with(|p| p.borrow_mut().extend(ops.iter().cloned()));
                    return Err(primary);
                }
                Err(rollback_err) => {
                    #[cfg(target_family = "wasm")]
                    ic_cdk::trap(format!(
                        "gleaph-graph: federated label index sync failed and rollback failed (op error: {primary}; rollback: {rollback_err})"
                    ));
                    #[cfg(not(target_family = "wasm"))]
                    {
                        return Err(PlanQueryError::FederatedIndexCall {
                            op: "label_compensate",
                            detail: format!("primary: {primary}; rollback: {rollback_err}"),
                        });
                    }
                }
            }
        }
        applied.push(op.clone());
    }
    Ok(())
}
