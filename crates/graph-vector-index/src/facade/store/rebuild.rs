//! Bounded shadow-version rebuild lifecycle for production `nlist > 1` vector indexes
//! (ADR 0031 Slice 7, extended with the `Training` phase in Slice 8).
//!
//! A rebuild builds a *shadow* index version (`target = active + 1`) alongside the live active
//! version, dual-writes mutations into both (see [`super::mutation`]), and publishes by an atomic
//! `VectorIndexDef` flip. Every long-running phase is bounded and cursor-resumable so no single
//! message performs an O(N) sweep:
//!
//! - `Sampling` collects a bounded distinct candidate pool from live subjects (capped by the
//!   combined-state byte budget and the per-iteration distance-op budget).
//! - `Training` refines `nlist` centroids with deterministic k-means-lite over that pool (one
//!   bounded iteration per step), then writes the target centroids.
//! - `Building` shadows every live subject's vector into its nearest target partition.
//! - `publish` is O(1): it flips `def` + centroid metadata once completeness is established.
//! - `Cleaning` (post-publish) collapses `shadow_slot -> slot` and drops the old version's pages.
//! - `Aborting` (from `Building`/`ReadyToPublish`) clears `shadow_slot` and drops the shadow pages.
//!
//! Shadow state is never visible to `vector_search`: search resolves the live slot via
//! [`crate::records::SubjectMapEntry::current_slot_for`] against `def.active_index_version`, which is
//! the old version until the atomic publish.

use super::search::{assign_partition, decode_f32, encode_f32, l2_squared_f32, read_centroids_at};
use super::{
    MAX_NLIST, MAX_REBUILD_SAMPLE_LIMIT, MAX_REBUILD_STATE_BYTES, MAX_REBUILD_STATE_OVERHEAD_BYTES,
    MAX_REBUILD_STEP_VECTOR_BYTES, MAX_REBUILD_STEP_WORK, MAX_REBUILD_TRAINING_DISTANCE_OPS,
    MAX_REBUILD_TRAINING_ITERATIONS, VectorIndexStore,
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
    VectorEncoding, VectorIndexError, VectorMetric, VectorPartitionHealthSummary,
    VectorRebuildPhase, VectorRebuildStatus,
};
use ic_stable_structures::storable::Storable;
use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Bound;

/// Clamps a caller-supplied per-step work budget to `1..=MAX_REBUILD_STEP_WORK`, so a Router that
/// passes a huge value (e.g. `u32::MAX`) cannot force an O(N) scan/drop in one message and a `0`
/// value still makes forward progress (ADR 0031 Slice 7).
fn clamp_step_work(requested: u32) -> u32 {
    requested.clamp(1, MAX_REBUILD_STEP_WORK)
}

/// Whether a rebuild's `Training` phase is feasible within the bounded-state and bounded-per-message
/// contracts for the given target `nlist`/`stride_bytes`/`dims` (ADR 0031 Slice 8). Both must hold;
/// `admin_start_vector_rebuild` rejects with `InvalidRebuildParams` otherwise:
///
/// - **Combined-state (P2):** `2 * nlist * stride_bytes + MAX_REBUILD_STATE_OVERHEAD_BYTES <=
///   MAX_REBUILD_STATE_BYTES`. The `+ overhead` term matches the `candidate_pool_cap` reservation,
///   guaranteeing the pool can hold `>= nlist` candidates alongside the trained centroids.
/// - **Per-iteration work (P1):** `nlist * nlist * dims <= MAX_REBUILD_TRAINING_DISTANCE_OPS`, so
///   `>= nlist` candidates can be sampled and one k-means-lite iteration over them stays within the
///   per-message op budget.
fn training_start_feasible(nlist: u32, stride_bytes: u32, dims: u16) -> bool {
    let nlist = nlist as u64;
    let state_ok = nlist
        .checked_mul(stride_bytes as u64)
        .and_then(|x| x.checked_mul(2))
        .and_then(|x| x.checked_add(MAX_REBUILD_STATE_OVERHEAD_BYTES))
        .is_some_and(|x| x <= MAX_REBUILD_STATE_BYTES);
    let ops_ok = nlist
        .checked_mul(nlist)
        .and_then(|x| x.checked_mul(dims as u64))
        .is_some_and(|x| x <= MAX_REBUILD_TRAINING_DISTANCE_OPS);
    state_ok && ops_ok
}

/// Bounded distinct candidate-pool size (count) for `Training`: the smaller of the byte-budget cap
/// (reserving `nlist` centroids + encoding overhead inside [`MAX_REBUILD_STATE_BYTES`], P2) and the
/// distance-op cap (so one iteration's `candidate_count * nlist * dims` stays within
/// [`MAX_REBUILD_TRAINING_DISTANCE_OPS`], P1). For any params accepted by `training_start_feasible`
/// this is `>= nlist` (ADR 0031 Slice 8).
fn candidate_pool_cap(nlist: u32, stride_bytes: u32, dims: u16) -> usize {
    let nlist = nlist as u64;
    let stride = (stride_bytes as u64).max(1);
    let dims = (dims as u64).max(1);
    let centroid_bytes = nlist.saturating_mul(stride);
    let pool_bytes = MAX_REBUILD_STATE_BYTES
        .saturating_sub(centroid_bytes)
        .saturating_sub(MAX_REBUILD_STATE_OVERHEAD_BYTES);
    let cap_by_bytes = pool_bytes / stride;
    let cap_by_ops = MAX_REBUILD_TRAINING_DISTANCE_OPS / nlist.saturating_mul(dims).max(1);
    cap_by_bytes.min(cap_by_ops) as usize
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
            training_iteration: 0,
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
            training_iteration: 0,
        },
        VectorRebuildStateRecord::Training {
            target_index_version,
            nlist,
            iteration,
            candidates,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Training,
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: 0,
            candidates_collected: u32::try_from(candidates.len()).unwrap_or(u32::MAX),
            training_iteration: *iteration,
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
            training_iteration: 0,
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
            training_iteration: 0,
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
            training_iteration: 0,
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
            training_iteration: 0,
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
            training_iteration: 0,
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
    /// Begins a rebuild (ADR 0031 Slice 7/8). **O(1)**: validates parameters — including the Slice 8
    /// `Training` feasibility checks (combined-state byte budget and per-iteration distance-op
    /// budget) — and enters `Sampling` without scanning subjects or writing centroids.
    /// Insufficient-data failure is detected later in the bounded `Sampling` phase.
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
        // Bound the combined durable rebuild state (candidate pool + trained centroids) and the
        // per-iteration `Training` work; both scale with `dims` via `stride_bytes` (ADR 0031 Slice
        // 8). `MAX_NLIST` alone bounds neither.
        if !training_start_feasible(nlist, def.stride_bytes, def.dims) {
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
            VectorRebuildStateRecord::Training { .. } => self.training_step(index_id, state)?,
            VectorRebuildStateRecord::Building { .. } => {
                self.building_step(index_id, state, max_subjects, max_vector_bytes)?
            }
            // ReadyToPublish/Cleaning/Aborting/Failed are not advanced by `step`.
            other => other,
        };
        // Fail-closed encoded-size guard (P2): `Training` is the only durable rebuild value whose
        // size scales with sampled data (`candidates + centroids`). Re-check the Candid-encoded
        // length before persisting *any* Training transition (`Sampling -> Training` and the
        // `Training -> Training` re-persist after each iteration) so Candid-overhead drift returns a
        // trap-free error and leaves the prior recoverable state, instead of persisting an oversized
        // value or trapping the message.
        if matches!(next, VectorRebuildStateRecord::Training { .. })
            && next.to_bytes().len() as u64 > MAX_REBUILD_STATE_BYTES
        {
            return Err(VectorIndexError::InvalidRebuildParams);
        }
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

    /// Bounded `Sampling` step (ADR 0031 Slice 8): examines up to `max_subjects` rows, accumulating a
    /// bounded distinct candidate pool (`candidate_pool_cap`) from live subjects. Once sampling is
    /// done (range exhausted, `sample_limit` consumed, or the pool cap reached) it transitions to
    /// `Training` if `>= nlist` distinct candidates were collected, else to `Failed`. No centroids
    /// are written here; `Training` writes them on its transition to `Building`.
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
        let pool_cap = candidate_pool_cap(nlist, def.stride_bytes, def.dims);

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

        // Distinct membership via a transient set seeded from the existing pool (P1): keeps the
        // per-step dedup cost ~O(total candidate bytes) instead of `candidates.contains` being
        // O(existing candidates * vector_width) for every new candidate. The set is heap-only; the
        // durable state stays `Vec<Vec<u8>>`.
        let mut pool_cap_reached = candidates.len() >= pool_cap;
        let mut seen: HashSet<Vec<u8>> = candidates.iter().cloned().collect();
        for bytes in live_bytes {
            if candidates.len() >= pool_cap {
                pool_cap_reached = true;
                break;
            }
            if seen.insert(bytes.clone()) {
                candidates.push(bytes);
                if candidates.len() >= pool_cap {
                    pool_cap_reached = true;
                }
            }
        }

        let budget_exhausted = subjects_scanned >= sample_limit as u64;
        let sampling_done = range_exhausted || budget_exhausted || pool_cap_reached;
        if sampling_done {
            if candidates.len() >= nlist as usize {
                // The fail-closed encoded-size guard (P2) is applied centrally in
                // `rebuild_step_inner` before persisting, covering this transition and every
                // subsequent `Training` re-persist uniformly.
                return Ok(VectorRebuildStateRecord::Training {
                    target_index_version,
                    nlist,
                    sample_limit,
                    iteration: 0,
                    candidates,
                    centroids: Vec::new(),
                });
            }
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

    /// Bounded deterministic k-means-lite `Training` step (ADR 0031 Slice 8). Performs exactly one
    /// full iteration over the bounded candidate pool per call: assigns each candidate to its nearest
    /// current centroid (ties to the lowest id, via the same rule as `assign_partition`), recomputes
    /// each centroid as the arithmetic mean of its members, and keeps a previous centroid unchanged
    /// for an empty cluster. The per-iteration work `candidate_count * nlist * dims` is bounded by
    /// [`MAX_REBUILD_TRAINING_DISTANCE_OPS`] (the `Sampling` pool cap); sums/counts are transient
    /// heap buffers (`O(nlist * dims)`), never persisted. After
    /// [`MAX_REBUILD_TRAINING_ITERATIONS`] iterations it writes exactly `nlist` centroids to
    /// `IVF_CENTROIDS` and transitions to `Building`.
    fn training_step(
        &self,
        index_id: u32,
        state: VectorRebuildStateRecord,
    ) -> Result<VectorRebuildStateRecord, VectorIndexError> {
        let VectorRebuildStateRecord::Training {
            target_index_version,
            nlist,
            sample_limit,
            iteration,
            candidates,
            mut centroids,
        } = state
        else {
            unreachable!("training_step called off Training");
        };
        let def = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&index_id))
            .ok_or(VectorIndexError::UnknownIndex)?;
        let dims = def.dims as usize;
        let nlist_usize = nlist as usize;

        // Iteration 0: seed centroids from the first `nlist` distinct candidates.
        if centroids.is_empty() {
            centroids = candidates.iter().take(nlist_usize).cloned().collect();
        }

        let decoded_centroids: Vec<Vec<f32>> = centroids.iter().map(|c| decode_f32(c)).collect();
        let mut sums: Vec<Vec<f32>> = vec![vec![0.0f32; dims]; nlist_usize];
        let mut counts: Vec<u64> = vec![0u64; nlist_usize];
        for cand in &candidates {
            let v = decode_f32(cand);
            let mut best = 0usize;
            let mut best_d = f32::INFINITY;
            for (p, centroid) in decoded_centroids.iter().enumerate() {
                let d = l2_squared_f32(centroid, &v);
                if d < best_d {
                    best_d = d;
                    best = p;
                }
            }
            for (acc, x) in sums[best].iter_mut().zip(v.iter()) {
                *acc += *x;
            }
            counts[best] += 1;
        }
        // Recompute each centroid as the mean; an empty cluster keeps its previous centroid.
        for p in 0..nlist_usize {
            if counts[p] == 0 {
                continue;
            }
            let inv = 1.0f32 / counts[p] as f32;
            let mean: Vec<f32> = sums[p].iter().map(|s| s * inv).collect();
            centroids[p] = encode_f32(&mean);
        }
        let iteration = iteration + 1;

        if iteration >= MAX_REBUILD_TRAINING_ITERATIONS {
            IVF_CENTROIDS.with_borrow_mut(|m| {
                for (p, bytes) in centroids.iter().enumerate() {
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

        Ok(VectorRebuildStateRecord::Training {
            target_index_version,
            nlist,
            sample_limit,
            iteration,
            candidates,
            centroids,
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

    /// Head-only partition-health summary for the active index version (ADR 0031 Slice 8).
    /// **O(`nlist`)** (bounded by [`MAX_NLIST`]): reads `0..nlist` `PartitionHead` rows of the active
    /// version, summing `live_len`/`page_count` and tracking the max `live_len`; it never scans
    /// pages. Integer-only raw counts; the caller derives `avg`/skew. Router-guarded `#[query]`.
    pub fn admin_vector_partition_health(
        &self,
        caller: Principal,
        index_id: u32,
    ) -> Result<VectorPartitionHealthSummary, VectorIndexError> {
        self.assert_router_caller(caller)?;
        let def = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&index_id))
            .ok_or(VectorIndexError::UnknownIndex)?;
        let nlist = def.nlist;
        let active = def.active_index_version;
        let mut partitions_examined = 0u32;
        let mut live_rows = 0u64;
        let mut page_count = 0u64;
        let mut max_partition_live_rows = 0u64;
        VECTOR_PARTITION_HEADS.with_borrow(|heads| {
            for p in 0..nlist {
                if let Some(head) = heads.get(&PartitionKey::new(index_id, active, p)) {
                    partitions_examined += 1;
                    live_rows = live_rows.saturating_add(head.live_len);
                    page_count = page_count.saturating_add(head.page_count);
                    max_partition_live_rows = max_partition_live_rows.max(head.live_len);
                }
            }
        });
        Ok(VectorPartitionHealthSummary {
            nlist,
            partitions_examined,
            live_rows,
            page_count,
            max_partition_live_rows,
        })
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
            VectorRebuildStateRecord::Sampling { .. }
            | VectorRebuildStateRecord::Training { .. }
            | VectorRebuildStateRecord::Failed { .. } => {
                // Nothing durable outside the rebuild-state row: `Sampling`/`Training` write no
                // pages, no shadow slots, and no `IVF_CENTROIDS` (Training centroids live in the
                // state record until the transition to `Building`). O(1) back to `Idle`.
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
    use super::{
        MAX_REBUILD_STATE_BYTES, MAX_REBUILD_STATE_OVERHEAD_BYTES, MAX_REBUILD_STEP_WORK,
        MAX_REBUILD_TRAINING_DISTANCE_OPS, candidate_pool_cap, clamp_step_work,
        training_start_feasible,
    };

    #[test]
    fn training_start_feasible_enforces_both_bounds() {
        // Tiny config: well within both the combined-state and op budgets.
        assert!(training_start_feasible(2, 16, 4));

        // Combined-state (P2): `2 * nlist * stride + overhead > MAX_REBUILD_STATE_BYTES` is rejected.
        let stride = 4 * 1050u32; // dims = 1050
        let nlist = 1024u32;
        assert!(
            2 * nlist as u64 * stride as u64 + MAX_REBUILD_STATE_OVERHEAD_BYTES
                > MAX_REBUILD_STATE_BYTES,
            "fixture must exceed the combined-state cap"
        );
        assert!(!training_start_feasible(nlist, stride, 1050));

        // Op budget (P1) in isolation: a small stride keeps the state cap satisfied while
        // `nlist^2 * dims` exceeds the per-iteration op budget. (`nlist` here is only used to drive
        // the pure check; the caller separately clamps `nlist <= MAX_NLIST`.)
        let nlist = 40_000u32;
        assert!(
            2 * nlist as u64 * 4 + MAX_REBUILD_STATE_OVERHEAD_BYTES <= MAX_REBUILD_STATE_BYTES,
            "fixture must satisfy the combined-state cap"
        );
        // dims = 1, so `nlist^2 * dims` is just `nlist^2`.
        assert!(
            nlist as u64 * nlist as u64 > MAX_REBUILD_TRAINING_DISTANCE_OPS,
            "fixture must exceed the op budget"
        );
        assert!(!training_start_feasible(nlist, 4, 1));
    }

    #[test]
    fn encoded_training_state_stays_within_cap() {
        use crate::records::VectorRebuildStateRecord;
        use ic_stable_structures::storable::Storable;
        // Near-worst case: a wide stride so a full candidate pool plus `nlist` centroids sits right
        // under the envelope. The Candid-encoded length (enum tag + vec-length + nested-vec
        // overhead) must still fit within `MAX_REBUILD_STATE_BYTES`, validating the overhead
        // reserve (ADR 0031 Slice 8, P2).
        let nlist = 16u32;
        let dims = 768u16;
        let stride = 4 * dims as u32;
        assert!(training_start_feasible(nlist, stride, dims));
        let pool = candidate_pool_cap(nlist, stride, dims);
        let candidates: Vec<Vec<u8>> = (0..pool)
            .map(|i| {
                let mut v = vec![0u8; stride as usize];
                v[0..4].copy_from_slice(&(i as u32).to_le_bytes());
                v
            })
            .collect();
        let centroids: Vec<Vec<u8>> = (0..nlist as usize)
            .map(|_| vec![0u8; stride as usize])
            .collect();
        let state = VectorRebuildStateRecord::Training {
            target_index_version: 2,
            nlist,
            sample_limit: 1_000_000,
            iteration: 0,
            candidates,
            centroids,
        };
        let encoded = state.to_bytes().len() as u64;
        assert!(
            encoded <= MAX_REBUILD_STATE_BYTES,
            "encoded Training state {encoded} exceeds cap {MAX_REBUILD_STATE_BYTES}"
        );
    }

    #[test]
    fn candidate_pool_cap_is_at_least_nlist_when_feasible() {
        // For any params accepted by `training_start_feasible`, the pool can hold `>= nlist`
        // candidates (so sampling can reach `Training` rather than always failing).
        for (nlist, stride, dims) in [(2u32, 16u32, 4u16), (16, 512, 128), (64, 3072, 768)] {
            assert!(training_start_feasible(nlist, stride, dims));
            assert!(
                candidate_pool_cap(nlist, stride, dims) >= nlist as usize,
                "pool cap below nlist for ({nlist}, {stride}, {dims})"
            );
        }
    }

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
