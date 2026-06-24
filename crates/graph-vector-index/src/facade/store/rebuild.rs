//! Bounded shadow-version rebuild lifecycle for production `nlist > 1` vector indexes
//! (ADR 0031 Slice 7).
//!
//! A rebuild builds a *shadow* index version (`target = active + 1`) alongside the live active
//! version, dual-writes mutations into both (see [`super::mutation`]), and publishes by an atomic
//! `VectorIndexDef` flip. Every long-running phase is bounded and cursor-resumable so no single
//! message performs an O(N) sweep:
//!
//! - `Sampling` collects exactly `nlist` distinct centroid candidates from live subjects.
//! - `Building` shadows every live subject's vector into its nearest target partition.
//! - `publish` is O(1): it flips `def` + centroid metadata once completeness is established.
//! - `Cleaning` (post-publish) collapses `shadow_slot -> slot` and drops the old version's pages.
//! - `Aborting` (from `Building`/`ReadyToPublish`) clears `shadow_slot` and drops the shadow pages.
//!
//! Shadow state is never visible to `vector_search`: search resolves the live slot via
//! [`crate::records::SubjectMapEntry::current_slot_for`] against `def.active_index_version`, which is
//! the old version until the atomic publish.

use super::search::{assign_partition, read_centroids_at};
use super::{
    MAX_NLIST, MAX_REBUILD_CANDIDATE_BYTES, MAX_REBUILD_SAMPLE_LIMIT,
    MAX_REBUILD_STEP_VECTOR_BYTES, MAX_REBUILD_STEP_WORK, VectorIndexStore,
};
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, VECTOR_ID_TO_SLOT, VECTOR_INDEX_DEFS, VECTOR_PAGE,
    VECTOR_PARTITION_HEADS, VECTOR_REBUILD_STATE, VECTOR_SUBJECT_TO_ID,
};
use crate::records::{
    IvfCentroidMeta, PageKey, PartitionKey, SlotRef, SubjectKey, VectorIdKey,
    VectorRebuildStateRecord,
};
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorEncoding, VectorIndexError, VectorMetric, VectorRebuildPhase, VectorRebuildStatus,
};
use ic_stable_structures::storable::Storable;
use std::borrow::Cow;
use std::ops::Bound;

/// Clamps a caller-supplied per-step work budget to `1..=MAX_REBUILD_STEP_WORK`, so a Router that
/// passes a huge value (e.g. `u32::MAX`) cannot force an O(N) scan/drop in one message and a `0`
/// value still makes forward progress (ADR 0031 Slice 7).
fn clamp_step_work(requested: u32) -> u32 {
    requested.clamp(1, MAX_REBUILD_STEP_WORK)
}

/// Reads the current rebuild state for an index (`Idle` when none is recorded). Shared with the
/// mutation path so dual-write can branch on the lifecycle phase.
pub(super) fn rebuild_state_of(index_id: u32) -> VectorRebuildStateRecord {
    VECTOR_REBUILD_STATE
        .with_borrow(|m| m.get(&index_id))
        .unwrap_or_default()
}

/// Persists a rebuild state, removing the row entirely for `Idle` so an inactive index keeps no
/// durable rebuild bytes.
fn put_rebuild_state(index_id: u32, state: VectorRebuildStateRecord) {
    if matches!(state, VectorRebuildStateRecord::Idle) {
        VECTOR_REBUILD_STATE.with_borrow_mut(|m| m.remove(&index_id));
    } else {
        VECTOR_REBUILD_STATE.with_borrow_mut(|m| m.insert(index_id, state));
    }
}

/// O(1) bounded scalar snapshot of a rebuild state for the admin status query.
fn status_of(state: &VectorRebuildStateRecord) -> VectorRebuildStatus {
    match state {
        VectorRebuildStateRecord::Idle => VectorRebuildStatus {
            phase: VectorRebuildPhase::Idle,
            target_index_version: 0,
            nlist: 0,
            subjects_processed: 0,
            candidates_collected: 0,
        },
        VectorRebuildStateRecord::Sampling {
            target_index_version,
            nlist,
            candidates,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Sampling,
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: 0,
            candidates_collected: u32::try_from(candidates.len()).unwrap_or(u32::MAX),
        },
        VectorRebuildStateRecord::Building {
            target_index_version,
            nlist,
            subjects_processed,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Building,
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: *subjects_processed,
            candidates_collected: 0,
        },
        VectorRebuildStateRecord::ReadyToPublish {
            target_index_version,
            nlist,
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::ReadyToPublish,
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: 0,
            candidates_collected: 0,
        },
        VectorRebuildStateRecord::Cleaning {
            old_nlist,
            target_index_version,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Cleaning,
            target_index_version: *target_index_version,
            nlist: *old_nlist,
            subjects_processed: 0,
            candidates_collected: 0,
        },
        VectorRebuildStateRecord::Aborting {
            target_index_version,
            target_nlist,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Aborting,
            target_index_version: *target_index_version,
            nlist: *target_nlist,
            subjects_processed: 0,
            candidates_collected: 0,
        },
        VectorRebuildStateRecord::Failed {
            target_index_version,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Failed,
            target_index_version: *target_index_version,
            nlist: 0,
            subjects_processed: 0,
            candidates_collected: 0,
        },
    }
}

/// Inclusive/exclusive lower bound for resuming a `VECTOR_SUBJECT_TO_ID` scan over one index.
fn subject_lower(index_id: u32, cursor: &Option<Vec<u8>>) -> Bound<SubjectKey> {
    match cursor {
        None => Bound::Included(SubjectKey::index_lower(index_id)),
        Some(bytes) => Bound::Excluded(SubjectKey::from_bytes(Cow::Borrowed(bytes))),
    }
}

/// Marker stored in a teardown `subject_cursor` once the subject sub-stage is exhausted, so the page
/// sub-stage can begin. An empty byte string is never a valid 13-byte `SubjectKey`.
fn subjects_done_marker() -> Option<Vec<u8>> {
    Some(Vec::new())
}

fn is_subjects_done(cursor: &Option<Vec<u8>>) -> bool {
    matches!(cursor, Some(bytes) if bytes.is_empty())
}

/// Range-deletes up to `max_work` pages of `(index_id, version)`, resuming after `cursor`. Returns
/// `(next_cursor, exhausted)`; `exhausted` is true once no more pages of `version` remain.
fn drop_version_pages(
    index_id: u32,
    version: u64,
    cursor: Option<Vec<u8>>,
    max_work: u32,
) -> (Option<Vec<u8>>, bool) {
    let mut to_remove: Vec<PageKey> = Vec::new();
    let mut last: Option<PageKey> = None;
    let mut exhausted = true;
    VECTOR_PAGE.with_borrow(|pages| {
        let lower = match &cursor {
            None => Bound::Included(PageKey::new(index_id, version, 0, 0)),
            Some(bytes) => Bound::Excluded(PageKey::from_bytes(Cow::Borrowed(bytes))),
        };
        for entry in pages.range((lower, Bound::Unbounded)) {
            let key = entry.key();
            if key.index_id != index_id || key.index_version != version {
                break;
            }
            if to_remove.len() as u32 >= max_work {
                exhausted = false;
                break;
            }
            to_remove.push(*key);
            last = Some(*key);
        }
    });
    VECTOR_PAGE.with_borrow_mut(|pages| {
        for key in &to_remove {
            pages.remove(key);
        }
    });
    let next = if exhausted {
        None
    } else {
        last.map(Storable::into_bytes)
    };
    (next, exhausted)
}

/// Deletes the `0..nlist` partition heads and centroids of `(index_id, version)`. O(`nlist`),
/// bounded by [`MAX_NLIST`]; called only once a version's pages are fully drained.
fn drop_version_heads_and_centroids(index_id: u32, version: u64, nlist: u32) {
    VECTOR_PARTITION_HEADS.with_borrow_mut(|heads| {
        for p in 0..nlist {
            heads.remove(&PartitionKey::new(index_id, version, p));
        }
    });
    IVF_CENTROIDS.with_borrow_mut(|centroids| {
        for p in 0..nlist {
            centroids.remove(&PartitionKey::new(index_id, version, p));
        }
    });
}

impl VectorIndexStore {
    /// Begins a rebuild (ADR 0031 Slice 7). **O(1)**: validates parameters and enters `Sampling`
    /// without scanning subjects or writing centroids. Insufficient-data failure is detected later
    /// in the bounded `Sampling` phase.
    pub fn admin_start_vector_rebuild(
        &self,
        caller: Principal,
        index_id: u32,
        nlist: u32,
        sample_limit: u32,
    ) -> Result<(), VectorIndexError> {
        self.assert_router_caller(caller)?;
        let def = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&index_id))
            .ok_or(VectorIndexError::UnknownIndex)?;
        if def.encoding != VectorEncoding::F32 || def.metric != VectorMetric::L2Squared {
            return Err(VectorIndexError::InvalidRebuildParams);
        }
        if !(2..=MAX_NLIST).contains(&nlist) {
            return Err(VectorIndexError::InvalidRebuildParams);
        }
        if sample_limit < nlist || sample_limit > MAX_REBUILD_SAMPLE_LIMIT {
            return Err(VectorIndexError::InvalidRebuildParams);
        }
        // Bound the durable `Sampling.candidates` size: `nlist * stride_bytes` (checked, since
        // `stride_bytes` scales with `dims`) must not exceed the candidate-byte budget.
        let candidate_bytes = nlist.checked_mul(def.stride_bytes);
        if candidate_bytes.is_none_or(|b| b > MAX_REBUILD_CANDIDATE_BYTES) {
            return Err(VectorIndexError::InvalidRebuildParams);
        }
        if !matches!(rebuild_state_of(index_id), VectorRebuildStateRecord::Idle) {
            return Err(VectorIndexError::RebuildAlreadyActive);
        }
        let target = def
            .active_index_version
            .checked_add(1)
            .ok_or(VectorIndexError::AllocatorOverflow)?;
        put_rebuild_state(
            index_id,
            VectorRebuildStateRecord::Sampling {
                target_index_version: target,
                nlist,
                sample_limit,
                cursor: None,
                subjects_scanned: 0,
                candidates: Vec::new(),
            },
        );
        Ok(())
    }

    /// Drives one bounded `Sampling`/`Building` step. Router resumes by calling this repeatedly until
    /// the phase reaches `ReadyToPublish`.
    pub fn admin_vector_rebuild_step(
        &self,
        caller: Principal,
        index_id: u32,
        max_subjects: u32,
    ) -> Result<VectorRebuildStatus, VectorIndexError> {
        self.assert_router_caller(caller)?;
        self.rebuild_step_inner(
            index_id,
            clamp_step_work(max_subjects),
            MAX_REBUILD_STEP_VECTOR_BYTES,
        )
    }

    /// Shared body for the rebuild step, dispatching on phase with explicit per-step budgets so the
    /// production endpoint (clamped count + [`MAX_REBUILD_STEP_VECTOR_BYTES`]) and tests (injected
    /// small budgets) share one code path.
    fn rebuild_step_inner(
        &self,
        index_id: u32,
        max_subjects: u32,
        max_vector_bytes: u64,
    ) -> Result<VectorRebuildStatus, VectorIndexError> {
        let state = rebuild_state_of(index_id);
        let next = match state {
            VectorRebuildStateRecord::Idle => return Err(VectorIndexError::NoActiveRebuild),
            VectorRebuildStateRecord::Sampling { .. } => {
                self.sampling_step(index_id, state, max_subjects, max_vector_bytes)?
            }
            VectorRebuildStateRecord::Building { .. } => {
                self.building_step(index_id, state, max_subjects, max_vector_bytes)?
            }
            // ReadyToPublish/Cleaning/Aborting/Failed are not advanced by `step`.
            other => other,
        };
        put_rebuild_state(index_id, next.clone());
        Ok(status_of(&next))
    }

    /// Test-only entry point that drives one rebuild step with injectable count/byte budgets, so a
    /// small fixture can exercise the bounded-step truncation (cursor/status survives) without
    /// seeding `MAX_REBUILD_STEP_WORK` rows.
    #[cfg(test)]
    pub(crate) fn rebuild_step_with_budget(
        &self,
        index_id: u32,
        max_subjects: u32,
        max_vector_bytes: u64,
    ) -> Result<VectorRebuildStatus, VectorIndexError> {
        self.rebuild_step_inner(index_id, max_subjects, max_vector_bytes)
    }

    /// Bounded `Sampling` step: examines up to `max_subjects` rows, collecting distinct centroid
    /// candidates from live subjects. Writes the `nlist` centroids and transitions to `Building`
    /// once enough are collected; transitions to `Failed` if the range or `sample_limit` budget is
    /// exhausted with `< nlist` distinct candidates.
    fn sampling_step(
        &self,
        index_id: u32,
        state: VectorRebuildStateRecord,
        max_subjects: u32,
        max_vector_bytes: u64,
    ) -> Result<VectorRebuildStateRecord, VectorIndexError> {
        let VectorRebuildStateRecord::Sampling {
            target_index_version,
            nlist,
            sample_limit,
            cursor,
            mut subjects_scanned,
            mut candidates,
        } = state
        else {
            unreachable!("sampling_step called off Sampling");
        };
        let def = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&index_id))
            .ok_or(VectorIndexError::UnknownIndex)?;
        let active = def.active_index_version;

        let mut examined = 0u32;
        let mut last_key: Option<SubjectKey> = None;
        let mut range_exhausted = true;
        let mut bytes_buffered = 0u64;
        let mut live_bytes: Vec<Vec<u8>> = Vec::new();
        VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
            for entry in subjects.range((subject_lower(index_id, &cursor), Bound::Unbounded)) {
                let key = entry.key();
                if key.index_id != index_id {
                    break;
                }
                if examined >= max_subjects {
                    range_exhausted = false;
                    break;
                }
                examined += 1;
                last_key = Some(*key);
                let value = entry.value();
                if value.deleted {
                    continue;
                }
                let Some(slot) = value.current_slot_for(active) else {
                    continue;
                };
                if subjects_scanned >= sample_limit as u64 {
                    range_exhausted = false;
                    break;
                }
                subjects_scanned += 1;
                if let Some(bytes) = self.read_slot_bytes(index_id, slot) {
                    bytes_buffered += bytes.len() as u64;
                    live_bytes.push(bytes);
                }
                // Bound transient heap bytes: break after buffering at least one vector once the
                // per-step byte budget is reached (cursor resumes from `last_key`).
                if bytes_buffered >= max_vector_bytes {
                    range_exhausted = false;
                    break;
                }
            }
        });

        for bytes in live_bytes {
            if candidates.len() >= nlist as usize {
                break;
            }
            if !candidates.contains(&bytes) {
                candidates.push(bytes);
            }
        }

        if candidates.len() == nlist as usize {
            IVF_CENTROIDS.with_borrow_mut(|m| {
                for (p, bytes) in candidates.iter().enumerate() {
                    m.insert(
                        PartitionKey::new(index_id, target_index_version, p as u32),
                        bytes.clone(),
                    );
                }
            });
            return Ok(VectorRebuildStateRecord::Building {
                target_index_version,
                nlist,
                cursor: None,
                subjects_processed: 0,
            });
        }

        let budget_exhausted = subjects_scanned >= sample_limit as u64;
        if range_exhausted || budget_exhausted {
            return Ok(VectorRebuildStateRecord::Failed {
                target_index_version,
                reason: "insufficient live vectors to form nlist distinct centroids".to_string(),
            });
        }

        Ok(VectorRebuildStateRecord::Sampling {
            target_index_version,
            nlist,
            sample_limit,
            cursor: last_key.map(Storable::into_bytes),
            subjects_scanned,
            candidates,
        })
    }

    /// Bounded `Building` step: shadows up to `max_subjects` still-live subjects into their nearest
    /// target partition. Transitions to `ReadyToPublish` once the subject range is exhausted.
    fn building_step(
        &self,
        index_id: u32,
        state: VectorRebuildStateRecord,
        max_subjects: u32,
        max_vector_bytes: u64,
    ) -> Result<VectorRebuildStateRecord, VectorIndexError> {
        let VectorRebuildStateRecord::Building {
            target_index_version,
            nlist,
            cursor,
            mut subjects_processed,
        } = state
        else {
            unreachable!("building_step called off Building");
        };
        let def = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&index_id))
            .ok_or(VectorIndexError::UnknownIndex)?;
        let active = def.active_index_version;
        let centroids = read_centroids_at(index_id, target_index_version, nlist, def.dims)
            .ok_or(VectorIndexError::RebuildIncomplete)?;

        let mut examined = 0u32;
        let mut last_key: Option<SubjectKey> = None;
        let mut range_exhausted = true;
        let mut bytes_buffered = 0u64;
        // (subject key, vector_id, generation, active bytes) for subjects still needing a shadow.
        let mut pending: Vec<(SubjectKey, u64, u64, Vec<u8>)> = Vec::new();
        VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
            for entry in subjects.range((subject_lower(index_id, &cursor), Bound::Unbounded)) {
                let key = entry.key();
                if key.index_id != index_id {
                    break;
                }
                if examined >= max_subjects {
                    range_exhausted = false;
                    break;
                }
                examined += 1;
                last_key = Some(*key);
                let value = entry.value();
                if value.deleted {
                    continue;
                }
                if value
                    .shadow_slot
                    .is_some_and(|s| s.index_version == target_index_version)
                {
                    continue; // already shadowed (e.g. by dual-write)
                }
                let Some(active_slot) = value.current_slot_for(active) else {
                    continue;
                };
                let Some(vector_id) = value.vector_id else {
                    continue;
                };
                let Some(bytes) = self.read_slot_bytes(index_id, active_slot) else {
                    continue;
                };
                bytes_buffered += bytes.len() as u64;
                pending.push((*key, vector_id, active_slot.generation, bytes));
                // Bound transient heap bytes: break after buffering at least one vector once the
                // per-step byte budget is reached (cursor resumes from `last_key`).
                if bytes_buffered >= max_vector_bytes {
                    range_exhausted = false;
                    break;
                }
            }
        });

        for (key, vector_id, generation, bytes) in pending {
            let partition = assign_partition(&centroids, &bytes);
            let shadow_slot = self.append_slot(
                index_id,
                target_index_version,
                partition,
                def.slots_per_page,
                vector_id,
                generation,
                bytes,
            );
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                if let Some(mut entry) = m.get(&key)
                    && !entry.deleted
                    && entry.vector_id == Some(vector_id)
                {
                    entry.shadow_slot = Some(shadow_slot);
                    m.insert(key, entry);
                }
            });
            subjects_processed += 1;
        }

        if range_exhausted {
            Ok(VectorRebuildStateRecord::ReadyToPublish {
                target_index_version,
                nlist,
            })
        } else {
            Ok(VectorRebuildStateRecord::Building {
                target_index_version,
                nlist,
                cursor: last_key.map(Storable::into_bytes),
                subjects_processed,
            })
        }
    }

    /// Reports the current rebuild status (O(1) scalar snapshot). Router-guarded `#[query]`.
    pub fn admin_vector_rebuild_status(
        &self,
        caller: Principal,
        index_id: u32,
    ) -> Result<VectorRebuildStatus, VectorIndexError> {
        self.assert_router_caller(caller)?;
        Ok(status_of(&rebuild_state_of(index_id)))
    }

    /// Atomically publishes a `ReadyToPublish` rebuild (ADR 0031 Slice 7). **O(1)**: completeness is
    /// an invariant held by `Building` + dual-write, so no live-subject scan is performed. Flips
    /// `def.active_index_version` + `nlist` and the centroid metadata in one step, then enters the
    /// bounded `Cleaning` teardown.
    pub fn admin_publish_vector_rebuild(
        &self,
        caller: Principal,
        index_id: u32,
    ) -> Result<(), VectorIndexError> {
        self.assert_router_caller(caller)?;
        let state = rebuild_state_of(index_id);
        let VectorRebuildStateRecord::ReadyToPublish {
            target_index_version,
            nlist,
        } = state
        else {
            return Err(VectorIndexError::RebuildNotReadyToPublish);
        };
        let mut def = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&index_id))
            .ok_or(VectorIndexError::UnknownIndex)?;
        // O(`nlist`) centroid presence check (bounded by MAX_NLIST); not a subject scan.
        if read_centroids_at(index_id, target_index_version, nlist, def.dims).is_none() {
            return Err(VectorIndexError::RebuildIncomplete);
        }
        let old_version = def.active_index_version;
        let old_nlist = def.nlist;

        def.active_index_version = target_index_version;
        def.nlist = nlist;
        VECTOR_INDEX_DEFS.with_borrow_mut(|defs| defs.insert(index_id, def));
        IVF_CENTROID_META.with_borrow_mut(|meta| {
            let epoch = meta.get(&index_id).map(|m| m.centroid_epoch).unwrap_or(0);
            meta.insert(
                index_id,
                IvfCentroidMeta {
                    centroid_ready: true,
                    centroid_epoch: epoch + 1,
                    trained_index_version: target_index_version,
                },
            );
        });
        put_rebuild_state(
            index_id,
            VectorRebuildStateRecord::Cleaning {
                old_version,
                old_nlist,
                target_index_version,
                subject_cursor: None,
                page_cursor: None,
            },
        );
        Ok(())
    }

    /// Aborts an in-flight rebuild. From `Sampling`/`Failed` (nothing persisted) it returns straight
    /// to `Idle` in O(1); from `Building`/`ReadyToPublish` it enters the bounded `Aborting` teardown.
    pub fn admin_abort_vector_rebuild(
        &self,
        caller: Principal,
        index_id: u32,
    ) -> Result<(), VectorIndexError> {
        self.assert_router_caller(caller)?;
        let state = rebuild_state_of(index_id);
        let next = match state {
            VectorRebuildStateRecord::Sampling { .. } | VectorRebuildStateRecord::Failed { .. } => {
                VectorRebuildStateRecord::Idle
            }
            VectorRebuildStateRecord::Building {
                target_index_version,
                nlist,
                ..
            }
            | VectorRebuildStateRecord::ReadyToPublish {
                target_index_version,
                nlist,
            } => VectorRebuildStateRecord::Aborting {
                target_index_version,
                target_nlist: nlist,
                subject_cursor: None,
                page_cursor: None,
            },
            VectorRebuildStateRecord::Idle
            | VectorRebuildStateRecord::Cleaning { .. }
            | VectorRebuildStateRecord::Aborting { .. } => {
                return Err(VectorIndexError::NoActiveRebuild);
            }
        };
        put_rebuild_state(index_id, next);
        Ok(())
    }

    /// Drives one bounded teardown step for both the post-publish `Cleaning` and the `Aborting`
    /// paths. Each call advances at most `max_work` subjects or pages and is cursor-resumable to
    /// `Idle`.
    pub fn admin_vector_rebuild_cleanup_step(
        &self,
        caller: Principal,
        index_id: u32,
        max_work: u32,
    ) -> Result<VectorRebuildStatus, VectorIndexError> {
        self.assert_router_caller(caller)?;
        let max_work = clamp_step_work(max_work);
        let state = rebuild_state_of(index_id);
        let next = match state {
            VectorRebuildStateRecord::Cleaning { .. } => {
                self.cleaning_step(index_id, state, max_work)
            }
            VectorRebuildStateRecord::Aborting { .. } => {
                self.aborting_step(index_id, state, max_work)
            }
            _ => return Err(VectorIndexError::NoActiveRebuild),
        };
        put_rebuild_state(index_id, next.clone());
        Ok(status_of(&next))
    }

    /// One bounded `Cleaning` step: stage 1 collapses `shadow_slot -> slot` per subject and repoints
    /// `VECTOR_ID_TO_SLOT`; stage 2 range-deletes the old version's pages, then its heads/centroids.
    fn cleaning_step(
        &self,
        index_id: u32,
        state: VectorRebuildStateRecord,
        max_work: u32,
    ) -> VectorRebuildStateRecord {
        let VectorRebuildStateRecord::Cleaning {
            old_version,
            old_nlist,
            target_index_version,
            subject_cursor,
            page_cursor,
        } = state
        else {
            unreachable!("cleaning_step called off Cleaning");
        };

        if !is_subjects_done(&subject_cursor) {
            let (next_cursor, exhausted) =
                self.collapse_subjects(index_id, target_index_version, subject_cursor, max_work);
            return VectorRebuildStateRecord::Cleaning {
                old_version,
                old_nlist,
                target_index_version,
                subject_cursor: if exhausted {
                    subjects_done_marker()
                } else {
                    next_cursor
                },
                page_cursor: None,
            };
        }

        let (next_page, exhausted) =
            drop_version_pages(index_id, old_version, page_cursor, max_work);
        if exhausted {
            drop_version_heads_and_centroids(index_id, old_version, old_nlist);
            VectorRebuildStateRecord::Idle
        } else {
            VectorRebuildStateRecord::Cleaning {
                old_version,
                old_nlist,
                target_index_version,
                subject_cursor,
                page_cursor: next_page,
            }
        }
    }

    /// One bounded `Aborting` step: stage 1 clears `shadow_slot` per subject; stage 2 range-deletes
    /// the shadow (target) version's pages, then its heads/centroids. Active state is untouched.
    fn aborting_step(
        &self,
        index_id: u32,
        state: VectorRebuildStateRecord,
        max_work: u32,
    ) -> VectorRebuildStateRecord {
        let VectorRebuildStateRecord::Aborting {
            target_index_version,
            target_nlist,
            subject_cursor,
            page_cursor,
        } = state
        else {
            unreachable!("aborting_step called off Aborting");
        };

        if !is_subjects_done(&subject_cursor) {
            let (next_cursor, exhausted) =
                self.clear_shadow_slots(index_id, target_index_version, subject_cursor, max_work);
            return VectorRebuildStateRecord::Aborting {
                target_index_version,
                target_nlist,
                subject_cursor: if exhausted {
                    subjects_done_marker()
                } else {
                    next_cursor
                },
                page_cursor: None,
            };
        }

        let (next_page, exhausted) =
            drop_version_pages(index_id, target_index_version, page_cursor, max_work);
        if exhausted {
            drop_version_heads_and_centroids(index_id, target_index_version, target_nlist);
            VectorRebuildStateRecord::Idle
        } else {
            VectorRebuildStateRecord::Aborting {
                target_index_version,
                target_nlist,
                subject_cursor,
                page_cursor: next_page,
            }
        }
    }

    /// Stage 1 of `Cleaning`: collapse `shadow_slot@target -> slot` for up to `max_work` subjects,
    /// repointing the `VECTOR_ID_TO_SLOT` locator. Returns `(next_cursor, exhausted)`.
    fn collapse_subjects(
        &self,
        index_id: u32,
        target: u64,
        cursor: Option<Vec<u8>>,
        max_work: u32,
    ) -> (Option<Vec<u8>>, bool) {
        let mut examined = 0u32;
        let mut last_key: Option<SubjectKey> = None;
        let mut exhausted = true;
        let mut updates: Vec<(SubjectKey, SlotRef, Option<u64>)> = Vec::new();
        VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
            for entry in subjects.range((subject_lower(index_id, &cursor), Bound::Unbounded)) {
                let key = entry.key();
                if key.index_id != index_id {
                    break;
                }
                if examined >= max_work {
                    exhausted = false;
                    break;
                }
                examined += 1;
                last_key = Some(*key);
                let value = entry.value();
                if let Some(shadow) = value.shadow_slot
                    && shadow.index_version == target
                {
                    updates.push((*key, shadow, value.vector_id));
                }
            }
        });

        for (key, shadow, vector_id) in updates {
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                if let Some(mut entry) = m.get(&key) {
                    entry.slot = Some(shadow);
                    entry.shadow_slot = None;
                    m.insert(key, entry);
                }
            });
            if let Some(vector_id) = vector_id {
                VECTOR_ID_TO_SLOT
                    .with_borrow_mut(|m| m.insert(VectorIdKey::new(index_id, vector_id), shadow));
            }
        }

        (last_key.map(Storable::into_bytes), exhausted)
    }

    /// Stage 1 of `Aborting`: clear `shadow_slot@target` for up to `max_work` subjects without
    /// touching `slot` or the reverse-map locator. Returns `(next_cursor, exhausted)`.
    fn clear_shadow_slots(
        &self,
        index_id: u32,
        target: u64,
        cursor: Option<Vec<u8>>,
        max_work: u32,
    ) -> (Option<Vec<u8>>, bool) {
        let mut examined = 0u32;
        let mut last_key: Option<SubjectKey> = None;
        let mut exhausted = true;
        let mut keys: Vec<SubjectKey> = Vec::new();
        VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
            for entry in subjects.range((subject_lower(index_id, &cursor), Bound::Unbounded)) {
                let key = entry.key();
                if key.index_id != index_id {
                    break;
                }
                if examined >= max_work {
                    exhausted = false;
                    break;
                }
                examined += 1;
                last_key = Some(*key);
                let value = entry.value();
                if value.shadow_slot.is_some_and(|s| s.index_version == target) {
                    keys.push(*key);
                }
            }
        });

        for key in keys {
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                if let Some(mut entry) = m.get(&key) {
                    entry.shadow_slot = None;
                    m.insert(key, entry);
                }
            });
        }

        (last_key.map(Storable::into_bytes), exhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_REBUILD_STEP_WORK, clamp_step_work};

    #[test]
    fn clamp_step_work_bounds_caller_budget() {
        // A huge caller value (e.g. u32::MAX) is rounded down to the canister cap, so one step can
        // never perform an O(N) scan/drop.
        assert_eq!(clamp_step_work(u32::MAX), MAX_REBUILD_STEP_WORK);
        assert_eq!(
            clamp_step_work(MAX_REBUILD_STEP_WORK + 1),
            MAX_REBUILD_STEP_WORK
        );
        // A zero budget still makes forward progress.
        assert_eq!(clamp_step_work(0), 1);
        // In-range values pass through unchanged.
        assert_eq!(clamp_step_work(1), 1);
        assert_eq!(
            clamp_step_work(MAX_REBUILD_STEP_WORK),
            MAX_REBUILD_STEP_WORK
        );
        assert_eq!(clamp_step_work(100), 100);
    }
}
