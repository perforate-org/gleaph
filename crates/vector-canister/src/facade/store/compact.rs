//! Opt-in Router-driven slab dead-space reclamation (plan 0278).
//!
//! The `VECTOR_ROW_SLAB` is a pure bump allocator: replaced index versions and GC-drained pages
//! leave their bytes in place as dead space. This driver reclaims that space without touching the
//! append-only kernel (`ic-stable-vector-page-store`): a Router-driven bounded compaction copies
//! live pages into a dense prefix at the slab header, swapping each page's single
//! `VectorPageMeta.slab_offset` indirection after its bytes are persisted, then rewinds
//! `occupied_tail` once at finalize.
//!
//! **Snapshot range.** `admin_start_vector_slab_compact` records
//! `[SLAB_HEADER_SIZE, occupied_tail)` as the source range and fails if a compaction is already
//! active. With Slice 8 free-list reuse, pages appended after compaction start may land inside
//! the snapshot range (in a hole); they are moved like any other live page and their owners'
//! references updated. Records dropped mid-compaction by teardown are skipped by the scan.
//!
//! **Durability.** The whole driver state lives in one record (`VECTOR_SLAB_COMPACTION_STATE`,
//! MemoryId 11) carrying `{write_cursor, range_end, scan_cursor, pages_moved}`, so an interrupted
//! or upgraded canister resumes fail-closed from the persisted cursors.
//!
//! **Termination.** Each step either moves at least one page, advances the directory lap cursor,
//! or finalizes; a full lap with no in-range page proves exhaustion.

use super::authorization::assert_router_caller;
use super::{MAX_COMPACT_STEP_BYTES, MAX_COMPACT_STEP_PAGES};
use crate::facade::stable::PAGE_STORE;
use crate::facade::stable::VECTOR_SLAB_COMPACTION_STATE;
use crate::records::VectorSlabCompactionState;
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorSlabCompactionPhase, VectorSlabCompactionStatus,
};
use ic_stable_vector_page_store::SLAB_HEADER_SIZE;

/// Reads the durable compaction state (`Idle` when none is recorded).
fn compaction_state() -> VectorSlabCompactionState {
    VECTOR_SLAB_COMPACTION_STATE.with_borrow(|cell| *cell.get())
}

fn put_compaction_state(state: VectorSlabCompactionState) {
    VECTOR_SLAB_COMPACTION_STATE.with_borrow_mut(|cell| cell.set(state));
}

/// Bounded scalar snapshot for the admin surface.
fn status_of(
    phase: VectorSlabCompactionPhase,
    write_cursor: u64,
    range_end: u64,
    pages_moved: u64,
) -> VectorSlabCompactionStatus {
    VectorSlabCompactionStatus {
        phase,
        write_cursor,
        range_end,
        pages_moved,
    }
}

/// Begins a slab compaction. **O(1)**: validates no compaction is active and snapshots the source
/// range `[slab header end, occupied_tail)`; no scan runs here. Router-guarded `#[update]`.
pub(crate) fn admin_start_vector_slab_compact(
    caller: Principal,
) -> Result<(), VectorCanisterError> {
    assert_router_caller(caller)?;
    let free_head = match compaction_state() {
        VectorSlabCompactionState::Idle { free_head } => free_head,
        VectorSlabCompactionState::Compacting { .. } => {
            return Err(VectorCanisterError::CompactionAlreadyActive);
        }
    };
    let range_end = PAGE_STORE.with_borrow(|store| store.occupied_tail());
    put_compaction_state(VectorSlabCompactionState::Compacting {
        write_cursor: SLAB_HEADER_SIZE as u64,
        range_end,
        scan_cursor: None,
        pages_moved: 0,
        free_head,
    });
    Ok(())
}

/// Advances one bounded compaction unit: one meta-map lap segment plus at most one copy batch
/// (bytes persisted before each meta swap), finalizing — the gap assertion plus the single
/// `occupied_tail` rewind — when a full lap finds nothing live inside the snapshot range.
/// Budgets are clamped to `1..=MAX_COMPACT_STEP_PAGES` / `1..=MAX_COMPACT_STEP_BYTES` so a huge
/// caller value still cannot force an unbounded message. Router-guarded `#[update]`.
pub(crate) fn admin_vector_slab_compact_step(
    caller: Principal,
    max_pages: u32,
    max_bytes: u64,
) -> Result<VectorSlabCompactionStatus, VectorCanisterError> {
    assert_router_caller(caller)?;
    let entry_budget = max_pages.clamp(1, MAX_COMPACT_STEP_PAGES);
    let byte_budget = max_bytes.clamp(1, MAX_COMPACT_STEP_BYTES);

    let VectorSlabCompactionState::Compacting {
        write_cursor,
        range_end,
        scan_cursor,
        pages_moved,
        ..
    } = compaction_state()
    else {
        return Err(VectorCanisterError::NoActiveCompaction);
    };

    let outcome = PAGE_STORE.with_borrow_mut(|store| {
        store.compact_step(
            write_cursor,
            range_end,
            scan_cursor,
            entry_budget,
            byte_budget,
        )
    })?;
    let total_pages_moved = pages_moved + outcome.pages_moved;
    // Re-read the free-block anchor afterwards: the step itself may have consumed destination
    // blocks or registered reclaimed ones through the same record.
    let free_head = crate::facade::stable::slab_free_anchor_get();

    if outcome.finalized {
        put_compaction_state(VectorSlabCompactionState::Idle { free_head });
        Ok(status_of(
            VectorSlabCompactionPhase::Idle,
            outcome.write_cursor,
            range_end,
            total_pages_moved,
        ))
    } else {
        put_compaction_state(VectorSlabCompactionState::Compacting {
            write_cursor: outcome.write_cursor,
            range_end,
            scan_cursor: outcome.scan_cursor,
            pages_moved: total_pages_moved,
            free_head,
        });
        Ok(status_of(
            VectorSlabCompactionPhase::Compacting,
            outcome.write_cursor,
            range_end,
            total_pages_moved,
        ))
    }
}

/// Reports the current compaction state (O(1) scalar snapshot). Router-guarded `#[query]`; an
/// idle store reports phase `Idle` with zeroed cursors.
pub(crate) fn admin_vector_slab_compact_status(
    caller: Principal,
) -> Result<VectorSlabCompactionStatus, VectorCanisterError> {
    assert_router_caller(caller)?;
    match compaction_state() {
        VectorSlabCompactionState::Idle { .. } => {
            Ok(status_of(VectorSlabCompactionPhase::Idle, 0, 0, 0))
        }
        VectorSlabCompactionState::Compacting {
            write_cursor,
            range_end,
            pages_moved,
            ..
        } => Ok(status_of(
            VectorSlabCompactionPhase::Compacting,
            write_cursor,
            range_end,
            pages_moved,
        )),
    }
}
