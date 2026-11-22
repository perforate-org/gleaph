//! Per-index retirement (ADR 0037 §Resource deletion, shared vector target).
//!
//! Every vector index of a graph lives on the graph's one vector canister, so dropping one index
//! cannot delete the canister: it purges that index's rows here instead. The definition row
//! (MemoryId 4) is the lifecycle owner — `Live → Retiring → Retired` — and the bounded purge keeps
//! no durable cursor of its own. The Router drives it from the cursor each step returns; a lost
//! cursor restarts from `None`, which repeats only idempotent removals:
//!
//! 1. **Begin** (`Live`, no cursor): refused while a rebuild row exists, because a rebuild owns a
//!    second generation of pages. Writes `Retiring`, which from then on hides the index from every
//!    reader and makes sync-op admission refuse upserts and acknowledge removes without effect.
//!    Drops the heap centroid cache entry and any maintenance scan state.
//! 2. **Subjects**: removes every subject of the index (and its deleted-list entry).
//! 3. **Pages**: drops the active generation's pages, then its heads and centroids, then the
//!    centroid metadata, and writes `Retired`.
//!
//! A never-seen index id is written `Retired` directly, so a late upsert cannot create it either.

use super::authorization::{SubjectPurge, assert_router_caller, purge_subjects_step};
use super::rebuild::{drop_version_heads_and_centroids, drop_version_pages, rebuild_state_of};
use super::{MAX_REBUILD_STEP_WORK, centroid_cache};
use crate::facade::stable::{IVF_CENTROID_META, VECTOR_MAINTENANCE_STATE, definition_store};
use crate::records::{VectorIndexDef, VectorIndexDefinitionEntry, VectorRebuildStateRecord};
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorIndexRetireCursor, VectorIndexRetireStep,
};

/// Router-only: runs one bounded step of `index_id`'s retirement, resuming from `resume`.
pub(crate) fn admin_retire_vector_index(
    caller: Principal,
    index_id: u32,
    resume: Option<VectorIndexRetireCursor>,
) -> Result<VectorIndexRetireStep, VectorCanisterError> {
    assert_router_caller(caller)?;
    retire_step_with_budget(index_id, resume, MAX_REBUILD_STEP_WORK)
}

pub(crate) fn retire_step_with_budget(
    index_id: u32,
    resume: Option<VectorIndexRetireCursor>,
    budget: u32,
) -> Result<VectorIndexRetireStep, VectorCanisterError> {
    const DONE: VectorIndexRetireStep = VectorIndexRetireStep { next: None };
    let def = match (definition_store::entry(index_id)?, resume.is_some()) {
        (Some(VectorIndexDefinitionEntry::Retired), _) => return Ok(DONE),
        (None, false) => {
            definition_store::put_entry(index_id, VectorIndexDefinitionEntry::Retired)?;
            return Ok(DONE);
        }
        (Some(VectorIndexDefinitionEntry::Live(def)), false) => {
            begin(index_id, def)?;
            def
        }
        (Some(VectorIndexDefinitionEntry::Retiring(def)), _) => def,
        (None | Some(VectorIndexDefinitionEntry::Live(_)), true) => {
            return Err(VectorCanisterError::StaleRetireCursor);
        }
    };

    let next = match resume.unwrap_or(VectorIndexRetireCursor::Subjects {
        resume_key: Vec::new(),
    }) {
        VectorIndexRetireCursor::Subjects { resume_key } => {
            let step = purge_subjects_step(SubjectPurge::Index(index_id), &resume_key, budget)?;
            Some(match step.resume_key {
                Some(resume_key) => VectorIndexRetireCursor::Subjects { resume_key },
                None => VectorIndexRetireCursor::Pages { resume_key: None },
            })
        }
        VectorIndexRetireCursor::Pages { resume_key } => {
            let (resume_key, exhausted) =
                drop_version_pages(index_id, def.active_index_version, resume_key, budget);
            if exhausted {
                finish(index_id, def)?;
                None
            } else {
                Some(VectorIndexRetireCursor::Pages { resume_key })
            }
        }
    };
    Ok(VectorIndexRetireStep { next })
}

fn begin(index_id: u32, def: VectorIndexDef) -> Result<(), VectorCanisterError> {
    if rebuild_state_of(index_id) != VectorRebuildStateRecord::Idle {
        return Err(VectorCanisterError::RebuildAlreadyActive);
    }
    definition_store::put_entry(index_id, VectorIndexDefinitionEntry::Retiring(def))?;
    centroid_cache::invalidate(index_id);
    VECTOR_MAINTENANCE_STATE.with_borrow_mut(|states| states.remove(&index_id));
    Ok(())
}

fn finish(index_id: u32, def: VectorIndexDef) -> Result<(), VectorCanisterError> {
    drop_version_heads_and_centroids(
        index_id,
        def.active_index_version,
        def.nlist,
        def.levels,
        def.nlist_fine,
    );
    IVF_CENTROID_META.with_borrow_mut(|meta| meta.remove(&index_id));
    definition_store::put_entry(index_id, VectorIndexDefinitionEntry::Retired)?;
    Ok(())
}
