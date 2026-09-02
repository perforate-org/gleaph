//! Bounded shadow-version rebuild lifecycle for production `nlist > 1` vector indexes
//! (ADR 0031 Slice 7, extended with the `Training` phase in Slice 8; pool storage per the ADR
//! 0033 implementation).
//!
//! A rebuild builds a *shadow* index version (`target = active + 1`) alongside the live active
//! version, dual-writes mutations into both (see [`super::mutation`]), and publishes by an atomic
//! `VectorIndexDef` flip. Every long-running phase is bounded and cursor-resumable so no single
//! message performs an O(N) sweep:
//!
//! - `Sampling` collects a bounded distinct candidate pool from live subjects (capped by the
//!   dedicated rebuild-pool region budget and the per-iteration distance-op budget). Candidates
//!   are appended to the durable raw pool region ([`crate::facade::stable::rebuild_pool`]); the
//!   lifecycle record carries only a `pool_len` scalar, so no step decodes or re-encodes the pool.
//! - `Training` refines `nlist` centroids with deterministic k-means-lite over that pool (one
//!   bounded iteration per step, rows read individually from the pool region; the centroid work
//!   area lives beside the rows), then writes the target centroids.
//! - `Building` shadows every live subject's vector into its nearest target partition.
//! - `publish` is O(1): it flips `def` + centroid metadata once completeness is established.
//! - `Cleaning` (post-publish) collapses `shadow_slot -> slot` and drops the old version's pages;
//!   completing it releases the pool region.
//! - `Aborting` (from `Building`/`ReadyToPublish`) clears `shadow_slot` and drops the shadow pages;
//!   entering it releases the pool region.
//!
//! Shadow state is never visible to `vector_search`: search resolves the live slot via
//! [`crate::records::FixedSubjectMapEntry::current_slot_for`] against `def.active_index_version`, which is
//! the old version until the atomic publish.

use super::authorization::assert_router_caller;
use super::mutation::{append_slot_batch, insert_subject_entry, shape_def_for, tombstone_slot};
use super::search::{
    assign_partition, decode_f32, encode_f32, read_centroids_at, read_coarse_centroids_at,
    read_leaf_children_at, stored_to_f32_bytes,
};
use super::{
    MAX_LEAVES, MAX_NLIST, MAX_REBUILD_SAMPLE_LIMIT, MAX_REBUILD_STEP_VECTOR_BYTES,
    MAX_REBUILD_STEP_WORK, MAX_REBUILD_TRAINING_DISTANCE_OPS, MAX_REBUILD_TRAINING_ITERATIONS,
};
use crate::facade::stable::definition_store;
use crate::facade::stable::page_store::PageScratch;
use crate::facade::stable::rebuild_pool;
use crate::facade::stable::region_store::RegionError;
use crate::facade::stable::subject_store::{self, SubjectScanPage};
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, PAGE_STORE, VECTOR_PARTITION_HEADS, VECTOR_REBUILD_STATE,
};
use crate::records::{
    FixedSubjectMapEntry, IvfCentroidMeta, LEVELS_FLAT, LEVELS_TWO, PageKey, PartitionHeadRecord,
    PartitionKey, RawRebuildState, RebuildCandidate, SlotRef, SubjectKey, SubjectScanCursor,
    SubjectScanScope, VectorIndexDef, VectorRebuildStateRecord,
};
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_EPS_BPS, VECTOR_EPS_BPS_INFINITY, VectorCanisterError, VectorEncoding,
    VectorMaintenancePolicy, VectorMaintenanceRecommendation, VectorMetric,
    VectorPartitionHealthStep, VectorPartitionHealthSummary, VectorPartitionPageHealth,
    VectorRebuildPhase, VectorRebuildStatus, VectorSlabStats, VectorSlabStatsStep, VectorSubject,
};
use ic_stable_linear_hash_map::{SLOTS_PER_BUCKET, ScanError};
use ic_stable_structures::Storable;
use ic_stable_vector_page_store::kernel::{l2_squared_f32, l2_squared_f32_early_exit};

use super::recommend_partition_maintenance;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

/// One subject awaiting its shadow row in the `Building` phase: key, owned subject entry, active
/// slot the bytes were read from, the active bytes, and the active row's aux (the `I8` scale).
type ShadowPendingRow = (SubjectKey, FixedSubjectMapEntry, SlotRef, Vec<u8>, [u8; 8]);

struct NextSubjectPage {
    page: SubjectScanPage,
    restarted: bool,
}

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

/// Clamps a caller-supplied per-step work budget to `1..=MAX_REBUILD_STEP_WORK`, so a Router that
/// passes a huge value (e.g. `u32::MAX`) cannot force an O(N) scan/drop in one message and a `0`
/// value still makes forward progress (ADR 0031 Slice 7).
fn clamp_step_work(requested: u32) -> u32 {
    requested.clamp(1, MAX_REBUILD_STEP_WORK)
}

/// Recomputes the head-only partition-health skew summary for `(index_id, active)` directly from the
/// authoritative `PartitionHead` rows. **O(`nlist`)** (bounded by [`MAX_NLIST`]), never scans pages.
/// Used both by [`admin_vector_partition_health`] and the rebuild trigger so the
/// skew signal is always derived from current state rather than caller-attested input.
pub(super) fn partition_health_summary(
    index_id: u32,
    nlist: u32,
    active: u64,
) -> VectorPartitionHealthSummary {
    let mut partitions_examined = 0u32;
    let mut live_rows = 0u64;
    let mut page_count = 0u64;
    let mut max_partition_live_rows = 0u64;
    VECTOR_PARTITION_HEADS.with_borrow(|heads| {
        for p in 0..nlist {
            let record = heads
                .get(&PartitionKey::new(index_id, active, p))
                .expect("partition head get");
            if let Some(PartitionHeadRecord::Head(head)) = record {
                partitions_examined += 1;
                live_rows = live_rows.saturating_add(head.live_len);
                page_count = page_count.saturating_add(head.page_count);
                max_partition_live_rows = max_partition_live_rows.max(head.live_len);
            }
        }
    });
    VectorPartitionHealthSummary {
        nlist,
        partitions_examined,
        live_rows,
        page_count,
        max_partition_live_rows,
    }
}

/// Whether a rebuild's `Training` phase is feasible within the bounded-region and bounded-per-message
/// contracts for the given flat target `nlist`/`dims` and the pool vs centroid widths (ADR 0031
/// Slice 8; pool storage relocated to the dedicated region by the ADR 0033 implementation). Both must
/// hold; `admin_start_vector_rebuild` rejects with `InvalidRebuildParams` otherwise:
///
/// - **Pool region (P2):** the dedicated raw pool region must host at least `nlist` candidate rows
///   (`pad_stride + aux` wide) plus `nlist` trained canonical-f32 centroids inside
///   [`rebuild_pool::REGION_BYTES`] — the physical bound that replaced the retired Candid-envelope
///   constraint.
/// - **Per-iteration work (P1):** `nlist * nlist * dims <= MAX_REBUILD_TRAINING_DISTANCE_OPS`, so
///   `>= nlist` candidates can be sampled and one k-means-lite iteration over them stays within the
///   per-message op budget.
///
/// Flat-wrapper feasibility check.
///
/// The exact Slice 8 contract (see [`training_start_feasible_shape`] with `nlist_fine = 1`). Kept
/// for the flat feasibility tests; production admission goes through the shape-aware variant.
#[cfg(test)]
fn training_start_feasible(nlist: u32, pool_stride: u32, centroid_stride: u32, dims: u16) -> bool {
    training_start_feasible_shape(nlist, 1, pool_stride, centroid_stride, dims)
}

/// Shape-aware feasibility (`training_start_feasible` with a two-level extension). The Training
/// centroid work area is shared by every level, so both budgets are evaluated at the **work**
/// centroid count `max(coarse, fine)`:
///
/// - P1 becomes `work² × dims <= MAX_REBUILD_TRAINING_DISTANCE_OPS`. For a two-level rebuild this
///   subsumes the worst-case fine-subtree iteration: a subtree holds at most the whole pool, whose
///   cap is `OPS / (work × dims)` rows, so one fine iteration costs at most
///   `(OPS / (work × dims)) × f × dims = OPS × f / work <= OPS`. Practically this admits
///   `f <= nlist` geometries and fail-closes larger branching factors.
/// - P2 reserves the per-row coarse-id area ([`rebuild_pool::COARSE_ID_WIDTH`]) only when the
///   rebuild is two-level, so flat capacities are byte-identical to the pre-Slice-5 layout.
fn training_start_feasible_shape(
    nlist: u32,
    nlist_fine: u32,
    pool_stride: u32,
    centroid_stride: u32,
    dims: u16,
) -> bool {
    let two_level = nlist_fine > 1;
    let work = nlist.max(nlist_fine) as u64;
    let ops_ok = work
        .checked_mul(work)
        .and_then(|x| x.checked_mul(dims as u64))
        .is_some_and(|x| x <= MAX_REBUILD_TRAINING_DISTANCE_OPS);
    let state_ok =
        rebuild_pool::pool_capacity_for(pool_stride, work as u32, centroid_stride, two_level)
            .is_some();
    ops_ok && state_ok
}

/// Hashes a candidate's whole `(stored bytes, aux)` pair for transient dedup membership. The
/// hash only gates an exact byte comparison, so a collision can never merge distinct vectors.
fn candidate_hash(stored: &[u8], aux: &[u8; 8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (stored, aux).hash(&mut hasher);
    hasher.finish()
}

/// Bounded distinct candidate-pool size (count) for `Training`: the smaller of the pool-region
/// capacity left after reserving `work_centroids` at canonical-f32 width (P2) and the distance-op
/// cap (so one iteration's `candidate_count * work * dims` stays within
/// [`MAX_REBUILD_TRAINING_DISTANCE_OPS`], P1). Each candidate freezes its pad-stride stored bytes
/// plus its aux. For any params accepted by `training_start_feasible` this is `>= nlist`
/// (ADR 0031 Slice 8), and it equals the physical slot capacity of the pool region, so sampling's
/// graceful cap can never overrun the region.
/// Flat-wrapper pool cap (see [`candidate_pool_cap_shape`] with `nlist_fine = 1`). Kept for the
/// flat feasibility tests; production sampling uses the shape-aware variant.
#[cfg(test)]
fn candidate_pool_cap(nlist: u32, pool_stride: u32, centroid_stride: u32, dims: u16) -> usize {
    candidate_pool_cap_shape(nlist, 1, pool_stride, centroid_stride, dims)
}

/// Shape-aware variant of [`candidate_pool_cap`] evaluating both budgets at
/// `max(coarse, fine)` (see [`training_start_feasible_shape`]).
fn candidate_pool_cap_shape(
    nlist: u32,
    nlist_fine: u32,
    pool_stride: u32,
    centroid_stride: u32,
    dims: u16,
) -> usize {
    let two_level = nlist_fine > 1;
    let work = nlist.max(nlist_fine);
    let dims64 = (dims as u64).max(1);
    let cap_by_bytes =
        rebuild_pool::pool_capacity_for(pool_stride, work, centroid_stride, two_level).unwrap_or(0);
    let cap_by_ops =
        MAX_REBUILD_TRAINING_DISTANCE_OPS / (work as u64).saturating_mul(dims64).max(1);
    cap_by_bytes.min(cap_by_ops) as usize
}

/// Reads the current rebuild state for an index (`Idle` when none is recorded). Shared with the
/// mutation path so dual-write can branch on the lifecycle phase.
pub(super) fn rebuild_state_of(index_id: u32) -> VectorRebuildStateRecord {
    let raw = {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _get_scope = bench_scope("rebuild_state_get");
        VECTOR_REBUILD_STATE.with_borrow(|m| m.get(&index_id))
    };
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _decode_scope = bench_scope("rebuild_state_decode");
        raw.map(|raw| VectorRebuildStateRecord::from_bytes(Cow::Owned(raw.0)))
            .unwrap_or_default()
    }
}

/// Persists a rebuild state, removing the row entirely for `Idle` so an inactive index keeps no
/// durable rebuild bytes.
fn put_rebuild_state(index_id: u32, state: VectorRebuildStateRecord) {
    if matches!(state, VectorRebuildStateRecord::Idle) {
        VECTOR_REBUILD_STATE.with_borrow_mut(|m| m.remove(&index_id));
    } else {
        VECTOR_REBUILD_STATE
            .with_borrow_mut(|m| m.insert(index_id, RawRebuildState(state.into_bytes())));
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
            pool_len,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Sampling,
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: 0,
            candidates_collected: *pool_len,
            training_iteration: 0,
        },
        VectorRebuildStateRecord::Training {
            target_index_version,
            nlist,
            iteration,
            pool_len,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::Training,
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: 0,
            candidates_collected: *pool_len,
            training_iteration: *iteration,
        },
        VectorRebuildStateRecord::TrainCoarse {
            target_index_version,
            nlist,
            iteration,
            pool_len,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::TrainCoarse,
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: 0,
            candidates_collected: *pool_len,
            training_iteration: *iteration,
        },
        VectorRebuildStateRecord::TrainFine {
            target_index_version,
            nlist,
            coarse_cursor,
            iteration,
            pool_len,
            ..
        } => VectorRebuildStatus {
            phase: VectorRebuildPhase::TrainFine {
                coarse_cursor: *coarse_cursor,
            },
            target_index_version: *target_index_version,
            nlist: *nlist,
            subjects_processed: 0,
            candidates_collected: *pool_len,
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
            ..
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

/// Marker stored in a teardown `subject_cursor` once the subject sub-stage is exhausted, so the page
/// sub-stage can begin. `slot == u64::MAX` is never a valid slot index (capacity is far below it).
fn subjects_done_marker(scope: SubjectScanScope) -> Option<SubjectScanCursor> {
    Some(SubjectScanCursor::done(scope))
}

fn is_subjects_done(cursor: &Option<SubjectScanCursor>) -> bool {
    cursor.as_ref().is_some_and(SubjectScanCursor::is_done)
}

const BUILDING_SCAN_SLOT_BUDGET: u64 = 64;

/// Reads a bounded physical subject-map page, restarting once when a split/reset invalidates the
/// durable cursor. Callers that may stop in the middle of a page use a one-slot budget; Building
/// derives a larger budget that cannot exceed its remaining subject/byte allowance. The result
/// marks a restart so `Sampling` can discard accumulation tied to the old geometry.
fn next_subject_page(
    scope: SubjectScanScope,
    cursor: Option<SubjectScanCursor>,
    physical_slot_budget: u64,
) -> Result<Option<NextSubjectPage>, VectorCanisterError> {
    let cursor = match cursor {
        Some(cursor) if cursor.is_done() => return Ok(None),
        Some(cursor) => cursor,
        None => subject_store::scan_start(scope).map_err(VectorCanisterError::from)?,
    };
    match subject_store::scan_step(scope, cursor, physical_slot_budget) {
        Ok(page) => Ok(Some(NextSubjectPage {
            page,
            restarted: false,
        })),
        Err(RegionError::Scan(ScanError::RestartRequired)) => {
            let fresh = subject_store::scan_start(scope).map_err(VectorCanisterError::from)?;
            subject_store::scan_step(scope, fresh, physical_slot_budget)
                .map(|page| {
                    Some(NextSubjectPage {
                        page,
                        restarted: true,
                    })
                })
                .map_err(VectorCanisterError::from)
        }
        Err(error) => Err(error.into()),
    }
}

/// Deterministic furthest-point (Maximin-D) centroid seeding over the candidate pool.
///
/// The first centroid is the candidate with the largest L2 norm (Katsavounidis et al. 1994
/// deterministic variant); each subsequent centroid is the candidate farthest from the already-chosen
/// set (ties broken by candidate index). This spreads the initial centroids across the pool, which
/// improves the k-means result and lets the early-convergence exit in [`training_step`]
/// fire sooner. The stored-form pool is decoded to canonical f32 once, transiently (the durable
/// candidates stay frozen); cost is `O(nlist * n * dims)` — roughly one training iteration — so it is
/// amortized by the reduction in iterations for separated data.
fn furthest_point_seed(
    candidates: &[RebuildCandidate],
    nlist: usize,
    def: &VectorIndexDef,
) -> Vec<Vec<u8>> {
    let n = candidates.len();
    if n == 0 {
        return Vec::new();
    }
    let decoded: Vec<Vec<u8>> = candidates
        .iter()
        .map(|c| stored_to_f32_bytes(def, &c.stored, &c.aux))
        .collect();
    if n <= nlist {
        return decoded;
    }
    let dims = def.dims;

    let zero: Vec<f32> = vec![0.0; dims as usize];
    let norms: Vec<f32> = decoded.iter().map(|c| l2_squared_f32(c, &zero)).collect();
    let mut dist = vec![f32::INFINITY; n];
    let mut chosen = vec![false; n];
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(nlist);

    // First centroid: max-L2-norm candidate (ties -> lowest index).
    let first = (0..n)
        .max_by(|&i, &j| norms[i].total_cmp(&norms[j]).then_with(|| j.cmp(&i)))
        .expect("non-empty candidates");
    chosen[first] = true;
    let mut chosen_count = 1usize;
    out.push(decoded[first].clone());
    let first_decoded = decode_f32(&decoded[first]);
    for i in 0..n {
        dist[i] = l2_squared_f32(&decoded[i], &first_decoded);
    }

    // Subsequent: candidate farthest from the chosen set (ties -> lowest index).
    while chosen_count < nlist {
        let next = (0..n)
            .filter(|&i| !chosen[i])
            .max_by(|&i, &j| dist[i].total_cmp(&dist[j]).then_with(|| j.cmp(&i)))
            .expect("more candidates than nlist");
        chosen[next] = true;
        chosen_count += 1;
        out.push(decoded[next].clone());
        let next_decoded = decode_f32(&decoded[next]);
        for i in 0..n {
            let d = l2_squared_f32(&decoded[i], &next_decoded);
            if d < dist[i] {
                dist[i] = d;
            }
        }
    }
    out
}

/// Deletes up to `max_work` page-meta entries of `(index_id, version)` from the slab page store,
/// resuming after `cursor`. Returns `(next_cursor, exhausted)`; `exhausted` is true once no more
/// pages of `version` remain. Slab bytes are left as dead space (ADR 0032: no tail rewind this
/// slice). `VECTOR_PARTITION_HEADS` is dropped separately by [`drop_version_heads_and_centroids`]
/// once a version's pages are fully drained, so heads may transiently outlive their page meta during
/// an interrupted teardown without being page-store corruption.
fn drop_version_pages(
    index_id: u32,
    version: u64,
    cursor: Option<Vec<u8>>,
    max_work: u32,
) -> (Option<Vec<u8>>, bool) {
    let progress = PAGE_STORE
        .with_borrow_mut(|store| store.drop_version_pages(index_id, version, cursor, max_work));
    (progress.cursor, progress.exhausted)
}

/// Deletes the `0..nlist` partition heads and centroids of `(index_id, version)`. O(`nlist`),
/// bounded by [`MAX_NLIST`]. Deletes the partition heads and centroids of `(index_id, version)`
/// for **both key levels** (Slice 5): the level-0 coarse keys `0..nlist` of a two-level generation
/// and every leaf key `0..leaves` (a flat generation's leaf count is its `nlist`). Since Slice 8,
/// leaf heads are already removed by the page teardown (`drop_version_pages` drains whole
/// partitions atomically), so leaf removals tolerate absence; coarse heads have no pages and are
/// always removed here. The counts come from the teardown record's frozen shape, so a shape
/// change between generations cannot strand keys.
fn drop_version_heads_and_centroids(
    index_id: u32,
    version: u64,
    nlist: u32,
    levels: u8,
    nlist_fine: u32,
) {
    let leaves = if levels == LEVELS_TWO {
        nlist.saturating_mul(nlist_fine)
    } else {
        nlist
    };
    VECTOR_PARTITION_HEADS.with_borrow_mut(|heads| {
        if levels == LEVELS_TWO {
            for p in 0..nlist {
                heads
                    .remove(&PartitionKey::coarse(index_id, version, p))
                    .expect("partition head remove");
            }
        }
        for p in 0..leaves {
            // Leaf heads are normally already gone (page teardown); tolerate absence.
            let _ = heads.remove(&PartitionKey::new(index_id, version, p));
        }
    });
    IVF_CENTROIDS.with_borrow_mut(|centroids| {
        if levels == LEVELS_TWO {
            for p in 0..nlist {
                centroids.remove(&PartitionKey::coarse(index_id, version, p));
            }
        }
        for p in 0..leaves {
            centroids.remove(&PartitionKey::new(index_id, version, p));
        }
    });
}

/// Begins a rebuild (ADR 0031 Slice 7/8). **O(1)**: validates parameters — including the Slice 8
/// `Training` feasibility checks (combined-state byte budget and per-iteration distance-op
/// budget) — and enters `Sampling` without scanning subjects or writing centroids.
/// Insufficient-data failure is detected later in the bounded `Sampling` phase.
pub(crate) fn admin_start_vector_rebuild(
    caller: Principal,
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
) -> Result<(), VectorCanisterError> {
    admin_start_vector_rebuild_with_fine(
        caller,
        index_id,
        nlist,
        sample_limit,
        None,
        None,
        None,
        None,
    )
}

/// Shape-aware rebuild start (Slice 5). `fine_nlist = None` starts the unchanged flat lifecycle;
/// `Some(f)` (with `f >= 2`) starts a two-level (`levels = 2`) rebuild whose target generation
/// holds `nlist` coarse centroids and `nlist * f` leaves packed as `coarse * f + fine`.
///
/// `code_tier = Some(true)` (Slice 6, ADR 0078) builds the shadow generation with the 1-bit
/// RaBitQ first-stage code tier; the public encoding never changes and the advertised result
/// quality stays the original tier. The flag is persisted in the rebuild-pool header at `begin`
/// and consumed by the transition into `Building`.
///
/// `eps_query_bps` / `eps_fine_bps` (Slice 9) freeze the target generation's per-level ε₂ pruning
/// in basis points (`0` = nearest-partition-only, [`VECTOR_EPS_BPS_INFINITY`] = full scan). They
/// are persisted in the rebuild-pool header at `begin` and consumed by `publish` when the def is
/// flipped; search reads them from the published def, never from the pool. `None` = `0` (legacy
/// pruning).
///
/// Two-level admission adds, on top of the flat checks:
/// - `f >= 2` (a single-child hierarchy is flat with extra storage);
/// - `nlist * f <= MAX_LEAVES` (u32 partition-id packing and per-generation head capacity);
/// - the shared work-area feasibility at `max(nlist, f)` (see [`training_start_feasible_shape`]):
///   both P1 and P2 are evaluated at the largest level, which also bounds the worst-case fine
///   subtree iteration (a subtree holds at most the whole pool).
///
/// A tier-on target additionally fails closed when its page shape cannot fit even one row per
/// page (`shape_def_for` → `InvalidRebuildParams`), so an unbuildable geometry is rejected before
/// any state is written.
#[allow(clippy::too_many_arguments)]
pub(crate) fn admin_start_vector_rebuild_with_fine(
    caller: Principal,
    index_id: u32,
    nlist: u32,
    sample_limit: u32,
    fine_nlist: Option<u32>,
    code_tier: Option<bool>,
    eps_query_bps: Option<u32>,
    eps_fine_bps: Option<u32>,
) -> Result<(), VectorCanisterError> {
    assert_router_caller(caller)?;
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    if !matches!(def.encoding, VectorEncoding::F32 | VectorEncoding::I8)
        || (def.metric != VectorMetric::L2Squared && def.metric != VectorMetric::Cosine)
    {
        return Err(VectorCanisterError::InvalidRebuildParams);
    }
    if !(2..=MAX_NLIST).contains(&nlist) {
        return Err(VectorCanisterError::InvalidRebuildParams);
    }
    if sample_limit < nlist || sample_limit > MAX_REBUILD_SAMPLE_LIMIT {
        return Err(VectorCanisterError::InvalidRebuildParams);
    }
    let (levels, nlist_fine) = match fine_nlist {
        None => (LEVELS_FLAT, 1),
        // A single-child hierarchy is flat with extra storage, not a meaningful shape.
        Some(f) if f < 2 => return Err(VectorCanisterError::InvalidRebuildParams),
        Some(f) => {
            let leaves = nlist
                .checked_mul(f)
                .ok_or(VectorCanisterError::InvalidRebuildParams)?;
            if leaves > MAX_LEAVES {
                return Err(VectorCanisterError::InvalidRebuildParams);
            }
            (LEVELS_TWO, f)
        }
    };
    // Bound the durable rebuild-pool region (candidate pool + trained centroid work area) and the
    // per-iteration `Training` work; both scale with `dims`. The pool freezes native stored rows
    // (the index's `pad_stride_bytes` width plus aux), the trained centroids are always canonical
    // f32 (`dims * 4` bytes each), and both share the region budget.
    if !training_start_feasible_shape(
        nlist,
        nlist_fine,
        def.pad_stride_bytes,
        u32::from(def.dims) * 4,
        def.dims,
    ) {
        return Err(VectorCanisterError::InvalidRebuildParams);
    }
    let code_tier = code_tier.unwrap_or(false);
    // Fail-closed ε₂ bps admission (Slice 9): a value other than the ∞ sentinel must not exceed
    // `MAX_VECTOR_EPS_BPS` (the threshold factor would otherwise degenerate toward a full walk and
    // blur the distinction from ∞). `None` = `0` (legacy pruning).
    let eps_query_bps = eps_query_bps.unwrap_or(0);
    let eps_fine_bps = eps_fine_bps.unwrap_or(0);
    for bps in [eps_query_bps, eps_fine_bps] {
        if bps != VECTOR_EPS_BPS_INFINITY && bps > MAX_VECTOR_EPS_BPS {
            return Err(VectorCanisterError::InvalidRebuildParams);
        }
    }
    // Fail-closed page-shape feasibility of the TARGET generation before any state is written:
    // the tier-on geometry shrinks `slots_per_page` and must fit at least one row per page.
    shape_def_for(&def, levels, nlist_fine, code_tier)
        .map_err(|_| VectorCanisterError::InvalidRebuildParams)?;
    if !matches!(rebuild_state_of(index_id), VectorRebuildStateRecord::Idle) {
        return Err(VectorCanisterError::RebuildAlreadyActive);
    }
    // The pool region is single-tenant: another index's in-flight rebuild serializes this start
    // behind its own. Abort or complete that rebuild first.
    if rebuild_pool::bound_index().is_some_and(|bound| bound != index_id) {
        return Err(VectorCanisterError::RebuildAlreadyActive);
    }
    let target = def
        .active_index_version
        .checked_add(1)
        .ok_or(VectorCanisterError::AllocatorOverflow)?;
    // Bind + zero the pool region before the lifecycle row exists; both writes commit atomically
    // with this message. The centroid work area hosts whichever level is training
    // (`max(nlist, nlist_fine)` sets), and a two-level pool reserves the coarse-id array.
    let work_centroids = nlist.max(nlist_fine);
    rebuild_pool::begin(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        u32::from(def.dims) * 4,
        levels == LEVELS_TWO,
        code_tier,
        eps_query_bps,
        eps_fine_bps,
    )?;
    put_rebuild_state(
        index_id,
        VectorRebuildStateRecord::Sampling {
            target_index_version: target,
            nlist,
            sample_limit,
            cursor: None,
            subjects_scanned: 0,
            pool_len: 0,
            levels,
            nlist_fine,
        },
    );
    Ok(())
}

/// Starts a rebuild only if caller-attested partition health crosses the supplied policy (ADR
/// 0031 Slice 9). The operator gathers the head-only skew summary
/// ([`admin_vector_partition_health`](Self::admin_vector_partition_health)) and the merged
/// page-meta tombstone health ([`admin_vector_partition_health_step`](Self::admin_vector_partition_health_step),
/// run to `exhausted`), then passes them back with a policy; this re-derives the recommendation
/// and, if not `Healthy`, begins the rebuild via [`admin_start_vector_rebuild`](Self::admin_start_vector_rebuild)
/// (no autonomous timer in this slice). The decided recommendation is always returned, so a
/// `Healthy` result is an explicit no-op rather than an error.
///
/// **Trust model.** The head-only skew summary is *recomputed here* from the authoritative
/// `PartitionHead` rows (O(`nlist`)), so a stale or foreign skew summary can never trip the
/// trigger. Only the page-meta tombstone health is *trusted admin input*, since proving its
/// completeness would require an unbounded scan (mirroring the no-snapshot-isolation contract of
/// [`VectorPartitionHealthStep`]). The freshness guard rejects page health attested against a
/// different generation: `attested_page_health.index_id`/`index_version` must equal the index's
/// current `active_index_version`, else [`VectorCanisterError::StaleMaintenanceHealth`].
///
/// **nlist resolution.** `target_nlist = Some(n)` rebuilds at `n`; `None` defaults to the current
/// `def.nlist` only when it is `>= 2`. A degenerate `def.nlist == 1` with no `target_nlist`
/// returns [`VectorCanisterError::InvalidRebuildParams`] (the underlying rebuild requires
/// `nlist >= 2`). All other parameter/feasibility validation and the active-rebuild guard are
/// delegated to [`admin_start_vector_rebuild`](Self::admin_start_vector_rebuild).
pub(crate) fn admin_start_vector_rebuild_if_recommended(
    caller: Principal,
    index_id: u32,
    attested_page_health: VectorPartitionPageHealth,
    policy: VectorMaintenancePolicy,
    target_nlist: Option<u32>,
    sample_limit: u32,
) -> Result<VectorMaintenanceRecommendation, VectorCanisterError> {
    assert_router_caller(caller)?;
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    // Freshness guard: reject page health attested against a different generation. The skew
    // summary needs no such guard because it is recomputed below from current heads.
    if attested_page_health.index_id != index_id
        || attested_page_health.index_version != def.active_index_version
    {
        return Err(VectorCanisterError::StaleMaintenanceHealth);
    }
    let summary = partition_health_summary(index_id, def.nlist, def.active_index_version);
    let recommendation = recommend_partition_maintenance(&summary, &attested_page_health, &policy)?;
    if matches!(recommendation, VectorMaintenanceRecommendation::Healthy) {
        return Ok(recommendation);
    }
    let effective_nlist = match target_nlist {
        Some(n) => n,
        None if def.nlist >= 2 => def.nlist,
        None => return Err(VectorCanisterError::InvalidRebuildParams),
    };
    admin_start_vector_rebuild(caller, index_id, effective_nlist, sample_limit)?;
    Ok(recommendation)
}

/// Drives one bounded `Sampling`/`Building` step. Router resumes by calling this repeatedly until
/// the phase reaches `ReadyToPublish`.
pub(crate) fn admin_vector_rebuild_step(
    caller: Principal,
    index_id: u32,
    max_subjects: u32,
) -> Result<VectorRebuildStatus, VectorCanisterError> {
    assert_router_caller(caller)?;
    rebuild_step_inner(
        index_id,
        clamp_step_work(max_subjects),
        MAX_REBUILD_STEP_VECTOR_BYTES,
    )
}

/// Shared body for the rebuild step, dispatching on phase with explicit per-step budgets so the
/// production endpoint (clamped count + [`MAX_REBUILD_STEP_VECTOR_BYTES`]) and tests (injected
/// small budgets) share one code path.
fn rebuild_step_inner(
    index_id: u32,
    max_subjects: u32,
    max_vector_bytes: u64,
) -> Result<VectorRebuildStatus, VectorCanisterError> {
    let state = {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_read_state");
        rebuild_state_of(index_id)
    };
    let next = match state {
        VectorRebuildStateRecord::Idle => return Err(VectorCanisterError::NoActiveRebuild),
        VectorRebuildStateRecord::Sampling { .. } => {
            sampling_step(index_id, state, max_subjects, max_vector_bytes)?
        }
        VectorRebuildStateRecord::Training { .. } => training_step(index_id, state)?,
        // Two-level pipeline: coarse k-means, then per-subtree fine jobs (Slice 5).
        VectorRebuildStateRecord::TrainCoarse { .. } => train_coarse_step(index_id, state)?,
        VectorRebuildStateRecord::TrainFine { .. } => train_fine_step(index_id, state)?,
        VectorRebuildStateRecord::Building { .. } => {
            building_step(index_id, state, max_subjects, max_vector_bytes)?
        }
        // ReadyToPublish/Cleaning/Aborting/Failed are not advanced by `step`.
        other => other,
    };
    // Status is read before the move into the persist block (it only needs the phase/cursor
    // summary, not ownership).
    let status = status_of(&next);
    if matches!(next, VectorRebuildStateRecord::Failed { .. }) {
        // The candidate pool is dead state once sampling failed; release it with the transition.
        rebuild_pool::release();
    }
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_persist_state");
        match next {
            VectorRebuildStateRecord::Idle => {
                VECTOR_REBUILD_STATE.with_borrow_mut(|m| m.remove(&index_id));
            }
            // The record is scalars-only since the pool moved to the dedicated region, so the
            // persist is a single small Candid encode + store (ADR 0033 implementation).
            next => {
                VECTOR_REBUILD_STATE
                    .with_borrow_mut(|m| m.insert(index_id, RawRebuildState(next.into_bytes())));
            }
        }
    }
    Ok(status)
}

/// Test-only entry point that drives one rebuild step with injectable count/byte budgets, so a
/// small fixture can exercise the bounded-step truncation (cursor/status survives) without
/// seeding `MAX_REBUILD_STEP_WORK` rows.
#[cfg(test)]
pub(crate) fn rebuild_step_with_budget(
    index_id: u32,
    max_subjects: u32,
    max_vector_bytes: u64,
) -> Result<VectorRebuildStatus, VectorCanisterError> {
    rebuild_step_inner(index_id, max_subjects, max_vector_bytes)
}

/// Bounded `Sampling` step (ADR 0031 Slice 8): examines up to `max_subjects` rows, accumulating a
/// bounded distinct candidate pool (`candidate_pool_cap`) from live subjects in the durable pool
/// region (ADR 0033 implementation). Once sampling is done (range exhausted, `sample_limit`
/// consumed, or the pool cap reached) it transitions to `Training` if `>= nlist` distinct
/// candidates were collected, else to `Failed`. No centroids are written here; `Training` writes
/// them on its transition to `Building`.
fn sampling_step(
    index_id: u32,
    state: VectorRebuildStateRecord,
    max_subjects: u32,
    max_vector_bytes: u64,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let VectorRebuildStateRecord::Sampling {
        target_index_version,
        nlist,
        sample_limit,
        cursor,
        mut subjects_scanned,
        pool_len,
        levels,
        nlist_fine,
    } = state
    else {
        unreachable!("sampling_step called off Sampling");
    };
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("rebuild_sampling");
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    let active = def.active_index_version;
    let two_level = nlist_fine > 1;
    // Candidates freeze native stored rows (pad-stride wide plus aux), while the trained
    // centroids are canonical f32, so both share the region budget at their own widths. The work
    // area and coarse-id reservation follow the target shape (see `training_start_feasible_shape`).
    let centroid_stride = u32::from(def.dims) * 4;
    let work_centroids = nlist.max(nlist_fine);
    let pool_cap = candidate_pool_cap_shape(
        nlist,
        nlist_fine,
        def.pad_stride_bytes,
        centroid_stride,
        def.dims,
    );
    // Fail-closed resume validation: the durable pool region must be bound to this rebuild and
    // hold exactly the length the lifecycle record claims, before any scan work runs.
    let opened = rebuild_pool::open(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        two_level,
    )
    .map_err(VectorCanisterError::from)?;
    if opened.pool_len != pool_len {
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }
    let mut durable_len: usize = pool_len as usize;

    let mut last_cursor: Option<SubjectScanCursor> = cursor.clone();
    let mut range_exhausted = false;
    let mut bytes_buffered = 0u64;
    let mut live_rows: Vec<RebuildCandidate> = Vec::new();
    let mut accepted: Vec<RebuildCandidate> = Vec::new();
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_sampling_scan");
        // Phase 1: walk the subject map and collect live subjects to sample, keyed by their
        // active page. The byte budget is charged by row stride so the buffered `live_bytes`
        // stays bounded; the actual bytes are read in phase 2 (mirrors the rebuild `Building`
        // scan and the search exact-scan).
        let mut to_read: Vec<(PageKey, SlotRef)> = Vec::new();
        let scope = SubjectScanScope::Sampling {
            index_id,
            target_index_version,
        };
        let mut scan_cursor = cursor;
        // The owner cursor budgets physical slots, while this phase budgets live subjects.
        // Keep one-slot pages so a logical subject limit never skips the remainder of a page;
        // a bucket block is the bounded translation between the two budgets.
        let physical_step_limit = max_subjects.max(1).saturating_mul(SLOTS_PER_BUCKET);
        for _ in 0..physical_step_limit {
            let Some(next) = next_subject_page(scope, scan_cursor.clone(), 1)? else {
                range_exhausted = true;
                last_cursor = None;
                break;
            };
            if next.restarted {
                subjects_scanned = 0;
                // The scan geometry changed, so the accumulated pool is tied to stale cursors:
                // discard the durable rows exactly as the old in-record pool was cleared.
                rebuild_pool::reset_pool_rows(
                    index_id,
                    def.pad_stride_bytes,
                    work_centroids,
                    centroid_stride,
                    two_level,
                )
                .map_err(VectorCanisterError::from)?;
                durable_len = 0;
                to_read.clear();
                bytes_buffered = 0;
            }
            let page = next.page;
            scan_cursor = (!page.exhausted).then(|| page.next_cursor.clone());
            last_cursor = scan_cursor.clone();
            if page.exhausted {
                range_exhausted = true;
            }
            for (key, value) in page.entries {
                if key.index_id != index_id || value.deleted {
                    continue;
                }
                let Some(slot) = value.current_slot_for(active) else {
                    continue;
                };
                if subjects_scanned >= sample_limit as u64 {
                    break;
                }
                subjects_scanned += 1;
                bytes_buffered += def.pad_stride_bytes as u64;
                to_read.push((
                    PageKey::new(
                        index_id,
                        slot.index_version as u64,
                        slot.partition_id,
                        slot.page_id as u64,
                    ),
                    slot,
                ));
            }
            if range_exhausted
                || subjects_scanned >= sample_limit as u64
                || bytes_buffered >= max_vector_bytes
            {
                break;
            }
        }
        // Phase 2: bulk-read each distinct page once into a reused `PageScratch`, then extract
        // each sampled row's bytes into `live_rows`. A slot at/after `row_count` or tombstoned
        // is dropped, matching the per-subject `read_row_bytes` `None` path.
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("rebuild_sampling_read");
            to_read.sort_by_key(|(page_key, _)| *page_key);
            let mut scratch = PageScratch::new();
            let mut i = 0usize;
            while i < to_read.len() {
                let page_key = to_read[i].0;
                let loaded =
                    PAGE_STORE.with_borrow(|store| store.load_page(page_key, &mut scratch));
                let mut end = i + 1;
                while end < to_read.len() && to_read[end].0 == page_key {
                    end += 1;
                }
                if loaded {
                    for (_, slot) in &to_read[i..end] {
                        // Single decode decides liveness and yields the aux bytes; `live_row_info`
                        // rejects an uninitialized slot (at/after `row_count`) and a tombstoned
                        // row, matching the per-subject `read_row_bytes` `None` path.
                        let Some(info) = scratch.live_row_info(slot.slot) else {
                            continue;
                        };
                        // Sampling freezes each row in its native stored form (bytes + aux
                        // scale); f32 exists only transiently inside seed/Training computation.
                        live_rows.push(RebuildCandidate {
                            stored: scratch.vec_slice(slot.slot).to_vec(),
                            aux: info.aux,
                        });
                    }
                }
                i = end;
            }
        }
    }

    // Distinct membership over the durable pool without cloning pool bytes (P1, ADR 0033
    // implementation): the existing rows are streamed once and reduced to `hash -> row indices`,
    // so a new candidate's dedup check hashes its own bytes and, on a hash hit only, re-reads the
    // few colliding rows for an exact `(stored bytes, aux)` comparison — identical quantized bytes
    // under different scales stay distinct vectors. Rows accepted earlier in the SAME step are
    // checked against the in-memory batch (their region slots are not written until the bulk
    // append below), so same-step duplicates are excluded exactly as cross-step ones were. The
    // maps are heap-only and transient.
    let mut pool_cap_reached = durable_len >= pool_cap;
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_sampling_dedup");
        let mut seen: HashMap<u64, Vec<u32>> =
            HashMap::with_capacity(durable_len.max(live_rows.len()) * 2);
        rebuild_pool::for_each_row(def.pad_stride_bytes, |index, stored, aux| {
            seen.entry(candidate_hash(stored, &aux))
                .or_default()
                .push(index);
        })
        .map_err(VectorCanisterError::from)?;
        // Hash gate over this step's accepted rows; the vec holds offsets into `accepted`.
        let mut accepted_by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
        for row in live_rows {
            if durable_len >= pool_cap {
                pool_cap_reached = true;
                break;
            }
            let hash = candidate_hash(&row.stored, &row.aux);
            let duplicate = seen.get(&hash).is_some_and(|indices| {
                indices
                    .iter()
                    .any(|&i| rebuild_pool::read_row(i, def.pad_stride_bytes) == row)
            }) || accepted_by_hash
                .get(&hash)
                .is_some_and(|offsets| offsets.iter().any(|&k| accepted[k] == row));
            if !duplicate {
                accepted_by_hash
                    .entry(hash)
                    .or_default()
                    .push(accepted.len());
                accepted.push(row);
                durable_len += 1;
                if durable_len >= pool_cap {
                    pool_cap_reached = true;
                }
            }
        }
    }
    // One bulk append persists the accepted rows and advances the durable pool length.
    if !accepted.is_empty() {
        rebuild_pool::append_rows(
            index_id,
            def.pad_stride_bytes,
            work_centroids,
            centroid_stride,
            two_level,
            &accepted,
        )
        .map_err(VectorCanisterError::from)?;
    }

    let budget_exhausted = subjects_scanned >= sample_limit as u64;
    let sampling_done = range_exhausted || budget_exhausted || pool_cap_reached;
    if sampling_done {
        if durable_len >= nlist as usize {
            // A two-level rebuild enters the split coarse→fine training pipeline; flat keeps the
            // single-level `Training` phase unchanged.
            return Ok(if two_level {
                VectorRebuildStateRecord::TrainCoarse {
                    target_index_version,
                    nlist,
                    nlist_fine,
                    sample_limit,
                    iteration: 0,
                    pool_len: durable_len as u32,
                }
            } else {
                VectorRebuildStateRecord::Training {
                    target_index_version,
                    nlist,
                    sample_limit,
                    iteration: 0,
                    pool_len: durable_len as u32,
                    levels,
                    nlist_fine,
                }
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
        cursor: last_cursor,
        subjects_scanned,
        pool_len: durable_len as u32,
        levels,
        nlist_fine,
    })
}

/// One deterministic k-means-lite iteration over `candidates` against the current centroid set
/// (ADR 0031 Slice 8). Assigns each candidate to its nearest centroid (ties to the lowest id, via
/// the same rule as `assign_partition`; centroid-level early exit), recomputes each centroid as
/// the arithmetic mean of its members (spherical renormalization for cosine; a zero-norm mean or
/// an empty cluster keeps the previous centroid), and reports whether the recomputed set equals
/// the input exactly — the assignment is stable, so k-means has converged and stopping early is
/// exact (a converged set reproduces itself). Pure: identical float semantics for flat `Training`
/// and every two-level level ([`train_coarse_step`]/[`train_fine_step`]). The per-iteration work
/// `candidate_count * centroids * dims` is bounded by [`MAX_REBUILD_TRAINING_DISTANCE_OPS`] via
/// the sampling pool cap; sums/counts are transient heap buffers, never persisted.
fn kmeans_lite_iteration(
    def: &VectorIndexDef,
    candidates: &[RebuildCandidate],
    mut centroids: Vec<Vec<u8>>,
) -> (Vec<Vec<u8>>, bool) {
    let dims = def.dims as usize;
    let prev_centroids = centroids.clone();
    let k = centroids.len();
    let mut sums: Vec<Vec<f32>> = vec![vec![0.0f32; dims]; k];
    let mut counts: Vec<u64> = vec![0u64; k];
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_training_assign");
        let decoded_centroids: Vec<Vec<f32>> = centroids.iter().map(|c| decode_f32(c)).collect();
        for cand in candidates {
            // Each candidate is decoded from its frozen stored form exactly once per iteration;
            // the assignment scores canonical f32 bytes against the f32 centroids. An `F32` row
            // already is canonical f32, so its bytes are borrowed without a copy; only an `I8`
            // row materializes a transient dequantized buffer.
            let v = match def.encoding {
                VectorEncoding::F32 => Cow::Borrowed(cand.stored.as_slice()),
                VectorEncoding::I8 => Cow::Owned(stored_to_f32_bytes(def, &cand.stored, &cand.aux)),
            };
            let mut best = 0usize;
            let mut best_d = f32::INFINITY;
            for (p, centroid) in decoded_centroids.iter().enumerate() {
                // Centroid-level early exit: a centroid whose partial L2 already exceeds the
                // running best cannot be the nearest (L2 partial sums are monotone), so skip it.
                // A tie does not trigger the strict-exceeds exit, preserving the lowest-id
                // tie-break; a non-finite centroid is skipped (a NaN distance never beats
                // `best_d`).
                let Some(d) = l2_squared_f32_early_exit(&v, centroid, best_d) else {
                    continue;
                };
                if d < best_d {
                    best_d = d;
                    best = p;
                }
            }
            for (acc, x) in sums[best].iter_mut().zip(v.as_chunks::<4>().0) {
                *acc += f32::from_le_bytes(*x);
            }
            counts[best] += 1;
        }
    }
    {
        // Recompute each centroid as the mean; an empty cluster keeps its previous centroid.
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_training_recompute");
        let is_cosine = def.metric == VectorMetric::Cosine;
        for p in 0..k {
            if counts[p] == 0 {
                continue;
            }
            let inv = 1.0f32 / counts[p] as f32;
            let mean: Vec<f32> = sums[p].iter().map(|s| s * inv).collect();
            // Spherical k-means (cosine): renormalize the mean direction to unit length so the
            // next L2 assignment is cosine-aware (L2² = 2 − 2cos on unit vectors). A zero-norm
            // mean (members cancel direction) keeps the previous centroid.
            let new_centroid: Vec<f32> = if is_cosine {
                let norm_sq: f32 = mean.iter().map(|x| x * x).sum();
                if norm_sq == 0.0 {
                    continue;
                }
                let inv_norm = 1.0 / norm_sq.sqrt();
                mean.iter().map(|x| x * inv_norm).collect()
            } else {
                mean
            };
            centroids[p] = encode_f32(&new_centroid);
        }
    }
    let converged = prev_centroids == centroids;
    (centroids, converged)
}

/// Bounded deterministic k-means-lite `Training` step (ADR 0031 Slice 8), the flat (`levels = 1`)
/// phase. Performs exactly one full iteration over the bounded candidate pool per call via
/// [`kmeans_lite_iteration`] (one bounded step per message, rows read individually from the pool
/// region; the centroid work area lives beside the rows), then writes exactly `nlist` centroids to
/// `IVF_CENTROIDS` on convergence or [`MAX_REBUILD_TRAINING_ITERATIONS`] and transitions to
/// `Building`. The two-level pipeline uses [`train_coarse_step`]/[`train_fine_step`] instead.
fn training_step(
    index_id: u32,
    state: VectorRebuildStateRecord,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let VectorRebuildStateRecord::Training {
        target_index_version,
        nlist,
        sample_limit,
        iteration,
        pool_len,
        levels,
        nlist_fine,
    } = state
    else {
        unreachable!("training_step called off Training");
    };
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("rebuild_training");
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    let nlist_usize = nlist as usize;
    let centroid_stride = u32::from(def.dims) * 4;

    // Fail-closed resume validation: the pool region must still be bound to this rebuild and hold
    // exactly the frozen pool the lifecycle record claims.
    let opened = rebuild_pool::open(
        index_id,
        def.pad_stride_bytes,
        nlist,
        centroid_stride,
        false,
    )
    .map_err(VectorCanisterError::from)?;
    if opened.pool_len != pool_len {
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }
    // Load the frozen pool rows (fixed-width binary reads; no Candid decode, ADR 0033
    // implementation).
    let candidates = rebuild_pool::load_rows(
        index_id,
        def.pad_stride_bytes,
        nlist,
        centroid_stride,
        false,
    )
    .map_err(VectorCanisterError::from)?;
    // The Training centroid work area lives beside the pool rows in the same region: empty before
    // seeding, exactly `nlist` centroids afterwards.
    let mut centroids = rebuild_pool::get_centroids(
        index_id,
        def.pad_stride_bytes,
        nlist,
        centroid_stride,
        false,
    )
    .map_err(VectorCanisterError::from)?;

    // Iteration 0: seed centroids via deterministic furthest-point selection (spread across the
    // pool) rather than the first `nlist` candidates, which can cluster together and stall
    // convergence. See [`kmeans_lite_iteration`] for the exact early-exit contract.
    if centroids.is_empty() {
        if iteration != 0 {
            // A resumed iteration > 0 must find its previous iteration's work area.
            return Err(VectorCanisterError::RebuildPoolInvalid);
        }
        centroids = furthest_point_seed(&candidates, nlist_usize, &def);
        rebuild_pool::put_centroids(
            index_id,
            def.pad_stride_bytes,
            nlist,
            centroid_stride,
            false,
            &centroids,
        )
        .map_err(VectorCanisterError::from)?;
    } else if centroids.len() != nlist_usize {
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }

    let (centroids, converged) = kmeans_lite_iteration(&def, &candidates, centroids);
    let iteration = iteration + 1;

    if converged || iteration >= MAX_REBUILD_TRAINING_ITERATIONS {
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
            levels,
            nlist_fine,
            // The shadow generation's code tier was persisted at `begin` (pool header, Slice 6).
            code_tier: opened.code_tier,
        });
    }

    // Persist the refined centroid work area beside the pool rows for the next iteration.
    rebuild_pool::put_centroids(
        index_id,
        def.pad_stride_bytes,
        nlist,
        centroid_stride,
        false,
        &centroids,
    )
    .map_err(VectorCanisterError::from)?;
    Ok(VectorRebuildStateRecord::Training {
        target_index_version,
        nlist,
        sample_limit,
        iteration,
        pool_len,
        levels,
        nlist_fine,
    })
}

/// Canonical-f32 componentwise mean over a subtree's frozen members, in pool-index order (the
/// deterministic accumulation order). Returns `None` for an empty member list.
pub(super) fn member_mean_bytes(
    def: &VectorIndexDef,
    candidates: &[RebuildCandidate],
) -> Option<Vec<u8>> {
    let dims = def.dims as usize;
    if candidates.is_empty() {
        return None;
    }
    let mut sums = vec![0.0f32; dims];
    for cand in candidates {
        let v = match def.encoding {
            VectorEncoding::F32 => Cow::Borrowed(cand.stored.as_slice()),
            VectorEncoding::I8 => Cow::Owned(stored_to_f32_bytes(def, &cand.stored, &cand.aux)),
        };
        for (acc, x) in sums.iter_mut().zip(v.as_chunks::<4>().0) {
            *acc += f32::from_le_bytes(*x);
        }
    }
    let inv = 1.0f32 / candidates.len() as f32;
    Some(encode_f32(
        &sums.iter().map(|s| s * inv).collect::<Vec<_>>(),
    ))
}

/// Slice 5 **empty/insufficient subtree rules**, applied when a fine subtree job completes.
///
/// - **Empty** (`members = 0`, so `trained` is empty and `member_mean` is `None`): every one of
///   the `f` leaf centroids is a copy of the subtree's coarse centroid (`coarse_centroid`).
/// - **Insufficient** (`0 < members < f`): k-means ran over `k = members` seeds, so slots
///   `0..k` hold trained centroids; each unfilled slot (ascending) receives a copy of the trained
///   centroid **nearest to the subtree's member mean** (L2², ties broken to the lowest slot id).
///
/// Both rules are deterministic and allocate no new geometry: duplicated leaves simply share a
/// centroid, ε₂ selection (`<=` threshold) always selects all duplicates together, and row
/// placement collapses onto the lowest duplicate id. Documented identically in
/// `design/index/vector-index.md`.
pub(super) fn complete_subtree_leaf_centroids(
    trained: Vec<Vec<u8>>,
    member_mean: Option<&[u8]>,
    coarse_centroid: &[u8],
    f: usize,
) -> Vec<Vec<u8>> {
    let Some(mean) = member_mean else {
        // Empty subtree: replicate the coarse centroid into every leaf slot.
        return vec![coarse_centroid.to_vec(); f];
    };
    let k = trained.len();
    debug_assert!(k >= 1 && k <= f);
    // The member mean is byte-encoded; decode once and score every trained centroid against it
    // with the same kernel the assignment paths use.
    let mean_query = decode_f32(mean);
    let mut out = trained;
    for _ in k..f {
        let mut best = 0usize;
        let mut best_d = f32::INFINITY;
        for (p, centroid) in out[..k].iter().enumerate() {
            let d = l2_squared_f32(centroid, &mean_query);
            if d < best_d {
                best_d = d;
                best = p;
            }
        }
        out.push(out[best].clone());
    }
    out
}

/// Reads one centroid (canonical f32 components) at an exact partition key, rejecting a
/// wrong-width payload. The key carries the full `(index_id, version, partition)` scope.
fn read_single_centroid(key: PartitionKey, dims: u16) -> Option<Vec<f32>> {
    let bytes = IVF_CENTROIDS.with_borrow(|m| m.get(&key))?;
    let centroid = decode_f32(&bytes);
    (centroid.len() == dims as usize).then_some(centroid)
}

/// Writes one subtree's `f` leaf centroids into `IVF_CENTROIDS` at the packed leaf ids
/// `[coarse * f, (coarse + 1) * f)`.
fn write_subtree_leaf_centroids(
    index_id: u32,
    version: u64,
    nlist_fine: u32,
    coarse: u32,
    leaves: &[Vec<u8>],
) {
    let base = coarse * nlist_fine;
    IVF_CENTROIDS.with_borrow_mut(|m| {
        for (rel, bytes) in leaves.iter().enumerate() {
            m.insert(
                PartitionKey::new(index_id, version, base + rel as u32),
                bytes.clone(),
            );
        }
    });
}

/// Two-level `TrainCoarse` step (Slice 5): k-means over the whole candidate pool at the coarse
/// count, byte-for-byte the same convergence rule, seeding, and one-iteration-per-step budget as
/// flat [`training_step`]. On completion the coarse centroids land on the **level-0** keys, the
/// shared centroid work area is reset for the differently-sized fine sets, and the lifecycle
/// enters `TrainFine` at coarse cursor `0`.
fn train_coarse_step(
    index_id: u32,
    state: VectorRebuildStateRecord,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let VectorRebuildStateRecord::TrainCoarse {
        target_index_version,
        nlist,
        nlist_fine,
        sample_limit,
        iteration,
        pool_len,
    } = state
    else {
        unreachable!("train_coarse_step called off TrainCoarse");
    };
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("rebuild_training");
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    let nlist_usize = nlist as usize;
    let work_centroids = nlist.max(nlist_fine);
    let centroid_stride = u32::from(def.dims) * 4;

    let opened = rebuild_pool::open(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        true,
    )
    .map_err(VectorCanisterError::from)?;
    if opened.pool_len != pool_len {
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }
    let candidates = rebuild_pool::load_rows(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        true,
    )
    .map_err(VectorCanisterError::from)?;
    let mut centroids = rebuild_pool::get_centroids(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        true,
    )
    .map_err(VectorCanisterError::from)?;

    if centroids.is_empty() {
        if iteration != 0 {
            return Err(VectorCanisterError::RebuildPoolInvalid);
        }
        centroids = furthest_point_seed(&candidates, nlist_usize, &def);
        rebuild_pool::put_centroids(
            index_id,
            def.pad_stride_bytes,
            work_centroids,
            centroid_stride,
            true,
            &centroids,
        )
        .map_err(VectorCanisterError::from)?;
    } else if centroids.len() != nlist_usize {
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }

    let (centroids, converged) = kmeans_lite_iteration(&def, &candidates, centroids);
    let iteration = iteration + 1;

    if converged || iteration >= MAX_REBUILD_TRAINING_ITERATIONS {
        IVF_CENTROIDS.with_borrow_mut(|m| {
            for (p, bytes) in centroids.iter().enumerate() {
                m.insert(
                    PartitionKey::coarse(index_id, target_index_version, p as u32),
                    bytes.clone(),
                );
            }
        });
        // The fine jobs seed sets of `nlist_fine` (which may differ from `nlist`); clear the
        // recorded length so the first fine seeding is accepted as fresh.
        rebuild_pool::reset_centroids(
            index_id,
            def.pad_stride_bytes,
            work_centroids,
            centroid_stride,
            true,
        )
        .map_err(VectorCanisterError::from)?;
        return Ok(VectorRebuildStateRecord::TrainFine {
            target_index_version,
            nlist,
            nlist_fine,
            sample_limit,
            coarse_cursor: 0,
            iteration: 0,
            pool_len,
        });
    }

    rebuild_pool::put_centroids(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        true,
        &centroids,
    )
    .map_err(VectorCanisterError::from)?;
    Ok(VectorRebuildStateRecord::TrainCoarse {
        target_index_version,
        nlist,
        nlist_fine,
        sample_limit,
        iteration,
        pool_len,
    })
}

/// One bounded pass persisting every pool row's nearest coarse subtree id (lowest-id tie-break,
/// same rule as `assign_partition`) into the durable coarse-id array. Costs one Training
/// iteration's worth of distance ops (`pool_len * nlist_coarse * dims`), charged to its own
/// message before any fine job runs; fine jobs then resume from exactly this membership without
/// rescoring the pool.
fn assign_pool_coarse_ids(
    index_id: u32,
    def: &VectorIndexDef,
    target_index_version: u64,
    nlist_coarse: u32,
) -> Result<(), VectorCanisterError> {
    let centroid_stride = u32::from(def.dims) * 4;
    let coarse: Vec<Vec<f32>> = (0..nlist_coarse)
        .map(|p| {
            read_single_centroid(
                PartitionKey::coarse(index_id, target_index_version, p),
                def.dims,
            )
            .ok_or(VectorCanisterError::RebuildIncomplete)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut ids: Vec<u32> = Vec::new();
    rebuild_pool::for_each_row(def.pad_stride_bytes, |_row_index, stored, aux| {
        let v = match def.encoding {
            VectorEncoding::F32 => Cow::Borrowed(stored),
            VectorEncoding::I8 => Cow::Owned(stored_to_f32_bytes(def, stored, &aux)),
        };
        ids.push(assign_partition(&coarse, &v));
    })
    .map_err(VectorCanisterError::from)?;
    rebuild_pool::put_coarse_ids(
        index_id,
        def.pad_stride_bytes,
        nlist_coarse,
        centroid_stride,
        &ids,
    )?;
    Ok(())
}

/// Two-level `TrainFine` step (Slice 5). Message units, in order:
///
/// 1. **Assignment pass** (once, when the durable `assigned_len` is still zero): every pool row's
///    nearest coarse id is persisted ([`assign_pool_coarse_ids`]); the message ends there.
/// 2. **One k-means-lite iteration** of the current subtree's job (`coarse_cursor`): the
///    subtree's member rows are gathered from the durable ids, seeded at iteration 0 with
///    `min(nlist_fine, members)` furthest-point centroids, and refined via
///    [`kmeans_lite_iteration`] — identical convergence rule and budget as flat `Training`.
/// 3. **Subtree completion**: on convergence or the iteration cap the subtree's `nlist_fine` leaf
///    centroids (after the [`complete_subtree_leaf_centroids`] empty/insufficient rules) are
///    written to the packed leaf-id range, the work area resets, and the cursor advances. The
///    last subtree transitions to `Building`.
///
/// Each job re-validates feasibility (one iteration over its actual members at width
/// `nlist_fine` within [`MAX_REBUILD_TRAINING_DISTANCE_OPS`]) — admission already bounds members
/// by the pool cap, so a violation indicates corruption and fails the rebuild closed.
fn train_fine_step(
    index_id: u32,
    state: VectorRebuildStateRecord,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let VectorRebuildStateRecord::TrainFine {
        target_index_version,
        nlist,
        nlist_fine,
        sample_limit,
        coarse_cursor,
        iteration,
        pool_len,
    } = state
    else {
        unreachable!("train_fine_step called off TrainFine");
    };
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("rebuild_training");
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    let work_centroids = nlist.max(nlist_fine);
    let centroid_stride = u32::from(def.dims) * 4;

    let opened = rebuild_pool::open(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        true,
    )
    .map_err(VectorCanisterError::from)?;
    if opened.pool_len != pool_len {
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }

    // Unit 1: the once-per-rebuild coarse assignment pass.
    if opened.assigned_len == 0 {
        assign_pool_coarse_ids(index_id, &def, target_index_version, nlist)?;
        return Ok(VectorRebuildStateRecord::TrainFine {
            target_index_version,
            nlist,
            nlist_fine,
            sample_limit,
            coarse_cursor,
            iteration,
            pool_len,
        });
    }

    if coarse_cursor >= nlist {
        // A resumed cursor beyond the last subtree is corruption, not progress.
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }

    // Gather this subtree's member row indices from the durable coarse-id array (a 4-byte-per-row
    // stream; the rows themselves are read individually below).
    let mut members: Vec<u32> = Vec::new();
    rebuild_pool::for_each_coarse_id(|row_index, coarse| {
        if coarse == coarse_cursor {
            members.push(row_index);
        }
    })
    .map_err(VectorCanisterError::from)?;

    // Per-job feasibility re-validation (see function doc).
    let job_ops = (members.len() as u64)
        .checked_mul(u64::from(nlist_fine))
        .and_then(|x| x.checked_mul(u64::from(def.dims)));
    if !job_ops.is_some_and(|x| x <= MAX_REBUILD_TRAINING_DISTANCE_OPS) {
        return Ok(VectorRebuildStateRecord::Failed {
            target_index_version,
            reason: "fine subtree job exceeds the per-iteration training budget".to_string(),
        });
    }

    let candidates: Vec<RebuildCandidate> = members
        .iter()
        .map(|&row_index| rebuild_pool::read_row(row_index, def.pad_stride_bytes))
        .collect();
    let seeded_k = nlist_fine.min(candidates.len() as u32) as usize;
    let mut centroids = rebuild_pool::get_centroids(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        true,
    )
    .map_err(VectorCanisterError::from)?;

    if centroids.is_empty() {
        if iteration != 0 {
            // A resumed iteration > 0 must find its previous iteration's work area.
            return Err(VectorCanisterError::RebuildPoolInvalid);
        }
        if candidates.is_empty() {
            // Empty-subtree rule: all leaves replicate the coarse centroid; no k-means runs.
            let coarse_centroid = read_single_centroid(
                PartitionKey::coarse(index_id, target_index_version, coarse_cursor),
                def.dims,
            )
            .ok_or(VectorCanisterError::RebuildIncomplete)?;
            let encoded = encode_f32(&coarse_centroid);
            write_subtree_leaf_centroids(
                index_id,
                target_index_version,
                nlist_fine,
                coarse_cursor,
                &vec![encoded; nlist_fine as usize],
            );
            return advance_after_subtree(
                target_index_version,
                nlist,
                nlist_fine,
                sample_limit,
                pool_len,
                coarse_cursor,
                opened.code_tier,
            );
        }
        centroids = furthest_point_seed(&candidates, seeded_k, &def);
        rebuild_pool::put_centroids(
            index_id,
            def.pad_stride_bytes,
            work_centroids,
            centroid_stride,
            true,
            &centroids,
        )
        .map_err(VectorCanisterError::from)?;
    } else if centroids.len() != seeded_k {
        return Err(VectorCanisterError::RebuildPoolInvalid);
    }

    let (centroids, converged) = kmeans_lite_iteration(&def, &candidates, centroids);
    let iteration = iteration + 1;

    if converged || iteration >= MAX_REBUILD_TRAINING_ITERATIONS {
        // Insufficient-subtree rule fills any missing slots deterministically.
        let leaves = complete_subtree_leaf_centroids(
            centroids,
            member_mean_bytes(&def, &candidates).as_deref(),
            // Only used by the empty case, which never reaches here.
            &[],
            nlist_fine as usize,
        );
        write_subtree_leaf_centroids(
            index_id,
            target_index_version,
            nlist_fine,
            coarse_cursor,
            &leaves,
        );
        rebuild_pool::reset_centroids(
            index_id,
            def.pad_stride_bytes,
            work_centroids,
            centroid_stride,
            true,
        )
        .map_err(VectorCanisterError::from)?;
        return advance_after_subtree(
            target_index_version,
            nlist,
            nlist_fine,
            sample_limit,
            pool_len,
            coarse_cursor,
            opened.code_tier,
        );
    }

    rebuild_pool::put_centroids(
        index_id,
        def.pad_stride_bytes,
        work_centroids,
        centroid_stride,
        true,
        &centroids,
    )
    .map_err(VectorCanisterError::from)?;
    Ok(VectorRebuildStateRecord::TrainFine {
        target_index_version,
        nlist,
        nlist_fine,
        sample_limit,
        coarse_cursor,
        iteration,
        pool_len,
    })
}

/// Advances the fine pipeline past a completed subtree: `Building` after the last one, otherwise
/// the next coarse cursor with a fresh iteration counter. `code_tier` is the shadow generation's
/// flag read from the pool header (Slice 6) and frozen into the `Building` record.
#[allow(clippy::too_many_arguments)]
fn advance_after_subtree(
    target_index_version: u64,
    nlist: u32,
    nlist_fine: u32,
    sample_limit: u32,
    pool_len: u32,
    completed_coarse: u32,
    code_tier: bool,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let next_cursor = completed_coarse + 1;
    if next_cursor >= nlist {
        return Ok(VectorRebuildStateRecord::Building {
            target_index_version,
            nlist,
            cursor: None,
            subjects_processed: 0,
            levels: LEVELS_TWO,
            nlist_fine,
            code_tier,
        });
    }
    Ok(VectorRebuildStateRecord::TrainFine {
        target_index_version,
        nlist,
        nlist_fine,
        sample_limit,
        coarse_cursor: next_cursor,
        iteration: 0,
        pool_len,
    })
}

/// Bounded `Building` step: shadows up to `max_subjects` still-live subjects into their nearest
/// target partition. Transitions to `ReadyToPublish` once the subject range is exhausted.
///
/// Slice 5 two-level assignment: each row first picks its nearest **coarse** centroid (lowest-id
/// tie-break), then the best **leaf** within that coarse's contiguous child range
/// `[c·f, (c+1)·f)` (shortest distance, lowest-id tie-break). Child ranges are read from stable
/// memory per distinct coarse in the batch, so the heap cost is one subtree set at a time rather
/// than every leaf of the generation. Flat assignment is unchanged.
fn building_step(
    index_id: u32,
    state: VectorRebuildStateRecord,
    max_subjects: u32,
    max_vector_bytes: u64,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let VectorRebuildStateRecord::Building {
        target_index_version,
        nlist,
        cursor,
        mut subjects_processed,
        levels,
        nlist_fine,
        code_tier,
    } = state
    else {
        unreachable!("building_step called off Building");
    };
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("rebuild_building");
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    let active = def.active_index_version;
    // Flat: the whole target centroid set. Two-level: only the coarse set is loaded here; leaf
    // child sets are read lazily per distinct coarse after the batch is grouped (below).
    let centroids = read_centroids_at(index_id, target_index_version, nlist, def.dims)
        .ok_or(VectorCanisterError::RebuildIncomplete)?;

    let mut last_cursor: Option<SubjectScanCursor> = cursor.clone();
    let mut range_exhausted = false;
    let mut bytes_buffered = 0u64;
    let mut pending: Vec<ShadowPendingRow> = Vec::new();
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_building_scan");
        // Phase 1: walk the subject map and collect subjects still needing a shadow, keyed by
        // their active page. The byte budget is charged by row stride (the bytes each shadow row
        // will carry) so the buffered `pending` stays bounded; the actual bytes are read in phase
        // 2.
        let mut to_read: Vec<(PageKey, SubjectKey, FixedSubjectMapEntry, SlotRef)> = Vec::new();
        let scope = SubjectScanScope::Building {
            index_id,
            target_index_version,
        };
        let mut scan_cursor = cursor;
        let subject_budget = max_subjects.max(1);
        let mut subjects_in_step = 0u32;
        for _ in 0..max_subjects.max(1) {
            let remaining_subjects = u64::from(subject_budget.saturating_sub(subjects_in_step));
            let remaining_bytes = max_vector_bytes.saturating_sub(bytes_buffered);
            let stride = u64::from(def.pad_stride_bytes).max(1);
            let byte_budget = (remaining_bytes / stride).max(1);
            let physical_slot_budget = remaining_subjects
                .min(byte_budget)
                .min(BUILDING_SCAN_SLOT_BUDGET);
            let Some(next) = next_subject_page(scope, scan_cursor.clone(), physical_slot_budget)?
            else {
                range_exhausted = true;
                last_cursor = None;
                break;
            };
            let page = next.page;
            scan_cursor = (!page.exhausted).then(|| page.next_cursor.clone());
            last_cursor = scan_cursor.clone();
            if page.exhausted {
                range_exhausted = true;
            }
            for (key, value) in page.entries {
                if key.index_id != index_id
                    || value.deleted
                    || value
                        .shadow_slot
                        .is_some_and(|s| s.index_version as u64 == target_index_version)
                {
                    continue;
                }
                let Some(active_slot) = value.current_slot_for(active) else {
                    continue;
                };
                bytes_buffered += def.pad_stride_bytes as u64;
                subjects_in_step += 1;
                to_read.push((
                    PageKey::new(
                        index_id,
                        active_slot.index_version as u64,
                        active_slot.partition_id,
                        active_slot.page_id as u64,
                    ),
                    key,
                    value,
                    active_slot,
                ));
            }
            if range_exhausted
                || subjects_in_step >= subject_budget
                || bytes_buffered >= max_vector_bytes
            {
                break;
            }
        }
        // Phase 2: bulk-read each distinct page once into a reused `PageScratch`, then extract
        // each row's bytes. A subject whose slot is at/after `row_count` or is tombstoned is
        // dropped, matching the per-subject `read_row_bytes` `None` path.
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("rebuild_building_read");
            to_read.sort_by_key(|(page_key, _, _, _)| *page_key);
            let mut scratch = PageScratch::new();
            let mut i = 0usize;
            while i < to_read.len() {
                let page_key = to_read[i].0;
                let loaded =
                    PAGE_STORE.with_borrow(|store| store.load_page(page_key, &mut scratch));
                let mut end = i + 1;
                while end < to_read.len() && to_read[end].0 == page_key {
                    end += 1;
                }
                if loaded {
                    for (_, key, entry, slot) in &to_read[i..end] {
                        // Single decode decides liveness and yields the aux bytes; `live_row_info`
                        // rejects an uninitialized slot (at/after `row_count`) and a tombstoned
                        // row, matching the per-subject `read_row_bytes` `None` path.
                        let Some(info) = scratch.live_row_info(slot.slot) else {
                            continue;
                        };
                        let bytes = scratch.vec_slice(slot.slot).to_vec();
                        pending.push((*key, *entry, *slot, bytes, info.aux));
                    }
                }
                i = end;
            }
        }
    }

    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("rebuild_building_append");
        // Shadow rows are written with the SHADOW generation's geometry (Slice 6 SSOT): derived
        // once per step from the same pure function the dual-write and publish paths use.
        let shadow_def = shape_def_for(&def, levels, nlist_fine, code_tier)?;
        // Pre-compute each row's nearest target partition and group by partition, so a whole
        // partition's shadow rows are appended in one batched page-store call (amortizing page
        // directory commits across rows) and each subject entry is updated with a single map
        // insert (no redundant get). The guard below is still checked per row because the
        // subject's live slot must be the one we read bytes from. Partition groups iterate in
        // ascending id order in both shapes.
        let two_level = levels == LEVELS_TWO;
        let grouped: Vec<(u32, Vec<ShadowPendingRow>)> = if two_level {
            // Stage 1: group the batch by nearest coarse.
            let mut by_coarse: BTreeMap<u32, Vec<ShadowPendingRow>> = BTreeMap::new();
            for (key, entry, active_slot, bytes, aux) in pending {
                // Coarse assignment is in f32 space, matching the upsert path so pre- and
                // post-rebuild assignment agree.
                let assign_bytes = stored_to_f32_bytes(&def, &bytes, &aux);
                let coarse = assign_partition(&centroids, &assign_bytes);
                by_coarse
                    .entry(coarse)
                    .or_default()
                    .push((key, entry, active_slot, bytes, aux));
            }
            // Stage 2: per distinct coarse, read its contiguous child range once and pick the
            // best leaf (lowest-id tie-break).
            let mut by_leaf: BTreeMap<u32, Vec<ShadowPendingRow>> = BTreeMap::new();
            for (coarse, bucket) in by_coarse {
                let children = read_leaf_children_at(
                    index_id,
                    target_index_version,
                    coarse,
                    nlist_fine,
                    def.dims,
                )
                .ok_or(VectorCanisterError::RebuildIncomplete)?;
                let base = coarse * nlist_fine;
                for (key, entry, active_slot, bytes, aux) in bucket {
                    let assign_bytes = stored_to_f32_bytes(&def, &bytes, &aux);
                    let rel = assign_partition(&children, &assign_bytes);
                    by_leaf.entry(base + rel).or_default().push((
                        key,
                        entry,
                        active_slot,
                        bytes,
                        aux,
                    ));
                }
            }
            by_leaf.into_iter().collect()
        } else {
            let mut by_partition: Vec<Vec<ShadowPendingRow>> =
                (0..nlist as usize).map(|_| Vec::new()).collect();
            for (key, entry, active_slot, bytes, aux) in pending {
                // Partition assignment is in f32 space (dequantizing an `I8` row with its aux scale),
                // matching the upsert `active_partition` so pre- and post-rebuild assignment agree.
                let assign_bytes = stored_to_f32_bytes(&def, &bytes, &aux);
                let partition = assign_partition(&centroids, &assign_bytes);
                by_partition[partition as usize].push((key, entry, active_slot, bytes, aux));
            }
            by_partition
                .into_iter()
                .enumerate()
                .map(|(p, b)| (p as u32, b))
                .filter(|(_, b)| !b.is_empty())
                .collect()
        };
        for (partition, bucket) in grouped {
            let shadow_slots = {
                let rows: Vec<(VectorSubject, &[u8], [u8; 8])> = bucket
                    .iter()
                    .map(|(key, _, _, bytes, aux)| (key.subject, bytes.as_slice(), *aux))
                    .collect();
                append_slot_batch(
                    index_id,
                    target_index_version,
                    partition,
                    &shadow_def,
                    &rows,
                )?
            };
            for (i, (shadow_slot, (key, mut entry, active_slot, _, _))) in
                shadow_slots.iter().copied().zip(bucket).enumerate()
            {
                // Positional stale-read guard: the subject's current live slot must still be the
                // one we read bytes from (replaces the retired `vector_id` equality check).
                if !entry.deleted && entry.current_slot_for(active) == Some(active_slot) {
                    entry.shadow_slot = Some(shadow_slot);
                    if let Err(error) = insert_subject_entry(key, entry) {
                        // The linked prefix belongs to the rebuilt subject map and stays live.
                        // This row and the unattempted suffix have no subject owner, so retire
                        // only those rows appended by this batch before returning the original
                        // commit error.
                        for unlinked_slot in &shadow_slots[i..] {
                            tombstone_slot(index_id, *unlinked_slot);
                        }
                        return Err(error);
                    }
                } else {
                    // The positional guard rejected this stale scan result, so its newly
                    // appended row has no subject owner.
                    tombstone_slot(index_id, shadow_slot);
                }
                subjects_processed += 1;
            }
        }
    }

    if range_exhausted {
        Ok(VectorRebuildStateRecord::ReadyToPublish {
            target_index_version,
            nlist,
            levels,
            nlist_fine,
            code_tier,
        })
    } else {
        Ok(VectorRebuildStateRecord::Building {
            target_index_version,
            nlist,
            cursor: last_cursor,
            subjects_processed,
            levels,
            nlist_fine,
            code_tier,
        })
    }
}

/// Reports the current rebuild status (O(1) scalar snapshot). Router-guarded `#[query]`.
pub(crate) fn admin_vector_rebuild_status(
    caller: Principal,
    index_id: u32,
) -> Result<VectorRebuildStatus, VectorCanisterError> {
    assert_router_caller(caller)?;
    Ok(status_of(&rebuild_state_of(index_id)))
}

/// Head-only partition-health summary for the active index version (ADR 0031 Slice 8).
/// **O(`nlist`)** (bounded by [`MAX_NLIST`]): reads `0..nlist` `PartitionHead` rows of the active
/// version, summing `live_len`/`page_count` and tracking the max `live_len`; it never scans
/// pages. Integer-only raw counts; the caller derives `avg`/skew. Router-guarded `#[query]`.
pub(crate) fn admin_vector_partition_health(
    caller: Principal,
    index_id: u32,
) -> Result<VectorPartitionHealthSummary, VectorCanisterError> {
    assert_router_caller(caller)?;
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    Ok(partition_health_summary(
        index_id,
        def.nlist,
        def.active_index_version,
    ))
}

/// Derived slab-space observability for the ADR 0032 page store (maintenance, not search truth).
/// Whole-slab physical facts are global; `index_id` (`None` = all indexes) scopes only the
/// logical row/referenced-byte counters and the version breakdown. Reads only `VECTOR_PAGE_META`
/// + the slab header — never row bytes or `VECTOR_SUBJECT_TO_ID`, and it mutates nothing.
///
/// **Unbounded** full page-meta scan (a bounded cursor snapshot is a deferred follow-up).
/// Router-guarded `#[query]`. It is purely derived, so an unknown/empty index yields zero scope
/// counters rather than an error.
pub(crate) fn admin_vector_slab_stats(
    caller: Principal,
    index_id: Option<u32>,
) -> Result<VectorSlabStats, VectorCanisterError> {
    assert_router_caller(caller)?;
    Ok(PAGE_STORE.with_borrow(|store| store.stats_for_index(index_id)))
}

/// IC-safe, cursor/budgeted variant of [`admin_vector_slab_stats`](Self::admin_vector_slab_stats)
/// for large stores: one bounded page-meta scan step (see [`VectorSlabStatsStep`] for the
/// client-side merge contract). Router-guarded `#[query]`. The cursor is external caller input,
/// so a malformed cursor returns [`VectorCanisterError::InvalidStatsCursor`] rather than trapping.
pub(crate) fn admin_vector_slab_stats_step(
    caller: Principal,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
    index_id: Option<u32>,
) -> Result<VectorSlabStatsStep, VectorCanisterError> {
    assert_router_caller(caller)?;
    PAGE_STORE.with_borrow(|store| store.stats_step(cursor, max_pages, index_id))
}

/// Bounded page-meta tombstone-health step for the active index version (ADR 0031 Slice 9).
/// Router-guarded `#[query]`. Complements the head-only [`admin_vector_partition_health`] skew
/// summary with the tombstone signal (`total_rows`/`physical_live_rows`/`tombstoned_rows`) that
/// requires a page-meta scan. Resolves the active version from `VECTOR_INDEX_DEFS` and forwards to
/// the slab store; the cursor is scope-checked against `(index_id, active_version)` and a
/// malformed/wrong-scope cursor returns [`VectorCanisterError::InvalidStatsCursor`] rather than
/// trapping. See [`VectorPartitionHealthStep`] for the additive client-side merge contract.
///
/// [`admin_vector_partition_health`]: Self::admin_vector_partition_health
pub(crate) fn admin_vector_partition_health_step(
    caller: Principal,
    index_id: u32,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> Result<VectorPartitionHealthStep, VectorCanisterError> {
    assert_router_caller(caller)?;
    let def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    PAGE_STORE.with_borrow(|store| {
        store.partition_page_health_step(index_id, def.active_index_version, cursor, max_pages)
    })
}

/// Atomically publishes a `ReadyToPublish` rebuild (ADR 0031 Slice 7). **O(1)**: completeness is
/// an invariant held by `Building` + dual-write, so no live-subject scan is performed. Flips
/// `def.active_index_version` + `nlist` and the centroid metadata in one step, then enters the
/// bounded `Cleaning` teardown.
pub(crate) fn admin_publish_vector_rebuild(
    caller: Principal,
    index_id: u32,
) -> Result<(), VectorCanisterError> {
    assert_router_caller(caller)?;
    let state = rebuild_state_of(index_id);
    let VectorRebuildStateRecord::ReadyToPublish {
        target_index_version,
        nlist,
        levels,
        nlist_fine,
        code_tier,
    } = state
    else {
        return Err(VectorCanisterError::RebuildNotReadyToPublish);
    };
    let mut def = definition_store::get(index_id)
        .map_err(VectorCanisterError::from)?
        .ok_or(VectorCanisterError::UnknownIndex)?;
    // Completeness check at the shadow generation (not a subject scan). A two-level generation
    // requires the level-0 coarse set **and** every packed leaf set; flat requires the leaf set.
    // This deliberately bypasses the heap centroid cache: the version-scope rule reserves the
    // cache for reads at the definition's active generation, and this read targets the pre-flip
    // shadow version.
    let leaves = if levels == LEVELS_TWO {
        nlist
            .checked_mul(nlist_fine)
            .ok_or(VectorCanisterError::RebuildIncomplete)?
    } else {
        nlist
    };
    let complete = read_centroids_at(index_id, target_index_version, leaves, def.dims).is_some()
        && (levels != LEVELS_TWO
            || read_coarse_centroids_at(index_id, target_index_version, nlist, def.dims).is_some());
    if !complete {
        return Err(VectorCanisterError::RebuildIncomplete);
    }
    let old_version = def.active_index_version;
    let old_nlist = def.nlist;
    // The old generation's shape is frozen into the Cleaning record for its teardown.
    let old_levels = def.levels;
    let old_nlist_fine = def.nlist_fine;

    def.active_index_version = target_index_version;
    def.nlist = nlist;
    def.levels = levels;
    def.nlist_fine = nlist_fine;
    // The whole target shape flips with the generation (Slices 5/6/8): the code tier changes the
    // frozen row width **and therefore `slots_per_page`**, so the published def must be derived
    // through the `shape_def_for` SSOT — never by patching flags onto the base def. Appends
    // derive their write layout from this def, so a stale capacity would corrupt every row.
    let published = shape_def_for(&def, levels, nlist_fine, code_tier)?;
    debug_assert_eq!(published.active_index_version, target_index_version);
    // The frozen per-level ε₂ pruning (Slice 9) is carried in the rebuild-pool header (persisted
    // at `begin`), not in the `Building`/`ReadyToPublish` lifecycle records — search reads it from
    // the published def, and the pool is released at `Cleaning` entry. Read it here so the
    // published def freezes the same values the rebuild started with. `shape_def_for` is the
    // physical-shape SSOT and deliberately does not widen for eps (eps is a search-time input,
    // not geometry), so the pool-derived values are set explicitly on the published def.
    let opened = rebuild_pool::open(
        index_id,
        def.pad_stride_bytes,
        nlist.max(nlist_fine),
        u32::from(def.dims) * 4,
        levels == LEVELS_TWO,
    )
    .map_err(VectorCanisterError::from)?;
    let mut published = published;
    published.eps_query_bps = opened.eps_query_bps;
    published.eps_fine_bps = opened.eps_fine_bps;
    definition_store::insert(index_id, published)
        .map(|_| ())
        .map_err(VectorCanisterError::from)?;
    // The active centroid set just changed generation; drop any warmed heap entry so search does
    // not serve stale centroids and the heap is freed (ADR 0031 Slice 9).
    super::centroid_cache::invalidate(index_id);
    IVF_CENTROID_META.with_borrow_mut(|meta| {
        meta.insert(
            index_id,
            IvfCentroidMeta {
                centroid_ready: true,
                trained_index_version: target_index_version,
            },
        );
    });
    put_rebuild_state(
        index_id,
        VectorRebuildStateRecord::Cleaning {
            old_version,
            old_nlist,
            old_levels,
            old_nlist_fine,
            target_index_version,
            subject_cursor: None,
            page_cursor: None,
        },
    );
    Ok(())
}

/// Aborts an in-flight rebuild. From `Sampling`/`Failed` it returns straight to `Idle` in O(1)
/// (releasing the pool region); from `Building`/`ReadyToPublish` it enters the bounded `Aborting`
/// teardown (also releasing the pool region at abort entry).
pub(crate) fn admin_abort_vector_rebuild(
    caller: Principal,
    index_id: u32,
) -> Result<(), VectorCanisterError> {
    assert_router_caller(caller)?;
    let state = rebuild_state_of(index_id);
    let next = match state {
        VectorRebuildStateRecord::Sampling { .. }
        | VectorRebuildStateRecord::Training { .. }
        | VectorRebuildStateRecord::TrainCoarse { .. }
        | VectorRebuildStateRecord::TrainFine { .. }
        | VectorRebuildStateRecord::Failed { .. } => {
            // No pages, no shadow slots, and no `IVF_CENTROIDS` were written; the pool region is
            // released with the transition back to `Idle`. O(1).
            rebuild_pool::release();
            VectorRebuildStateRecord::Idle
        }
        VectorRebuildStateRecord::Building {
            target_index_version,
            nlist,
            levels,
            nlist_fine,
            ..
        }
        | VectorRebuildStateRecord::ReadyToPublish {
            target_index_version,
            nlist,
            levels,
            nlist_fine,
            code_tier: _,
        } => {
            // The candidate pool is dead state from `Building` on; release it at abort entry
            // (ADR 0033 implementation teardown contract).
            rebuild_pool::release();
            VectorRebuildStateRecord::Aborting {
                target_index_version,
                target_nlist: nlist,
                target_levels: levels,
                target_nlist_fine: nlist_fine,
                subject_cursor: None,
                page_cursor: None,
            }
        }
        VectorRebuildStateRecord::Idle
        | VectorRebuildStateRecord::Cleaning { .. }
        | VectorRebuildStateRecord::Aborting { .. } => {
            return Err(VectorCanisterError::NoActiveRebuild);
        }
    };
    put_rebuild_state(index_id, next);
    Ok(())
}

/// Drives one bounded teardown step for both the post-publish `Cleaning` and the `Aborting`
/// paths. Each call advances at most `max_work` subjects or pages and is cursor-resumable to
/// `Idle`.
pub(crate) fn admin_vector_rebuild_cleanup_step(
    caller: Principal,
    index_id: u32,
    max_work: u32,
) -> Result<VectorRebuildStatus, VectorCanisterError> {
    assert_router_caller(caller)?;
    let max_work = clamp_step_work(max_work);
    let state = rebuild_state_of(index_id);
    let next = match state {
        VectorRebuildStateRecord::Cleaning { .. } => cleaning_step(index_id, state, max_work),
        VectorRebuildStateRecord::Aborting { .. } => aborting_step(index_id, state, max_work),
        _ => return Err(VectorCanisterError::NoActiveRebuild),
    }?;
    put_rebuild_state(index_id, next.clone());
    Ok(status_of(&next))
}

/// One bounded `Cleaning` step: stage 1 collapses `shadow_slot -> slot` per subject and repoints
/// `VECTOR_ID_TO_SLOT`; stage 2 range-deletes the old version's pages, then its heads/centroids.
fn cleaning_step(
    index_id: u32,
    state: VectorRebuildStateRecord,
    max_work: u32,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let VectorRebuildStateRecord::Cleaning {
        old_version,
        old_nlist,
        old_levels,
        old_nlist_fine,
        target_index_version,
        subject_cursor,
        page_cursor,
    } = state
    else {
        unreachable!("cleaning_step called off Cleaning");
    };

    if !is_subjects_done(&subject_cursor) {
        let scope = SubjectScanScope::Cleaning {
            index_id,
            target_index_version,
        };
        let (next_cursor, exhausted) =
            collapse_subjects(index_id, target_index_version, subject_cursor, max_work)?;
        return Ok(VectorRebuildStateRecord::Cleaning {
            old_version,
            old_nlist,
            old_levels,
            old_nlist_fine,
            target_index_version,
            subject_cursor: if exhausted {
                subjects_done_marker(scope)
            } else {
                next_cursor
            },
            page_cursor: None,
        });
    }

    let (next_page, exhausted) = drop_version_pages(index_id, old_version, page_cursor, max_work);
    if exhausted {
        drop_version_heads_and_centroids(
            index_id,
            old_version,
            old_nlist,
            old_levels,
            old_nlist_fine,
        );
        // Teardown complete: release the rebuild-pool region (ADR 0033 implementation lifecycle).
        rebuild_pool::release();
        Ok(VectorRebuildStateRecord::Idle)
    } else {
        Ok(VectorRebuildStateRecord::Cleaning {
            old_version,
            old_nlist,
            old_levels,
            old_nlist_fine,
            target_index_version,
            subject_cursor,
            page_cursor: next_page,
        })
    }
}

/// One bounded `Aborting` step: stage 1 clears `shadow_slot` per subject; stage 2 range-deletes
/// the shadow (target) version's pages, then its heads/centroids. Active state is untouched.
fn aborting_step(
    index_id: u32,
    state: VectorRebuildStateRecord,
    max_work: u32,
) -> Result<VectorRebuildStateRecord, VectorCanisterError> {
    let VectorRebuildStateRecord::Aborting {
        target_index_version,
        target_nlist,
        target_levels,
        target_nlist_fine,
        subject_cursor,
        page_cursor,
    } = state
    else {
        unreachable!("aborting_step called off Aborting");
    };

    if !is_subjects_done(&subject_cursor) {
        let scope = SubjectScanScope::Aborting {
            index_id,
            target_index_version,
        };
        let (next_cursor, exhausted) =
            clear_shadow_slots(index_id, target_index_version, subject_cursor, max_work)?;
        return Ok(VectorRebuildStateRecord::Aborting {
            target_index_version,
            target_nlist,
            target_levels,
            target_nlist_fine,
            subject_cursor: if exhausted {
                subjects_done_marker(scope)
            } else {
                next_cursor
            },
            page_cursor: None,
        });
    }

    let (next_page, exhausted) =
        drop_version_pages(index_id, target_index_version, page_cursor, max_work);
    if exhausted {
        drop_version_heads_and_centroids(
            index_id,
            target_index_version,
            target_nlist,
            target_levels,
            target_nlist_fine,
        );
        // Defensive: the pool region was already released at abort entry; keep the invariant
        // "teardown to `Idle` leaves no pool binding" against any interrupted older state.
        rebuild_pool::release();
        Ok(VectorRebuildStateRecord::Idle)
    } else {
        Ok(VectorRebuildStateRecord::Aborting {
            target_index_version,
            target_nlist,
            target_levels,
            target_nlist_fine,
            subject_cursor,
            page_cursor: next_page,
        })
    }
}

/// Stage 1 of `Cleaning`: collapse `shadow_slot@target -> slot` for up to `max_work` subjects.
/// Returns `(next_cursor, exhausted)`.
fn collapse_subjects(
    index_id: u32,
    target: u64,
    cursor: Option<SubjectScanCursor>,
    max_work: u32,
) -> Result<(Option<SubjectScanCursor>, bool), VectorCanisterError> {
    let mut updates: Vec<(SubjectKey, SlotRef)> = Vec::new();
    let scope = SubjectScanScope::Cleaning {
        index_id,
        target_index_version: target,
    };
    let mut scan_cursor = cursor;
    let mut exhausted = false;
    for _ in 0..max_work.max(1) {
        let Some(next) = next_subject_page(scope, scan_cursor.clone(), 1)? else {
            exhausted = true;
            break;
        };
        let page = next.page;
        scan_cursor = (!page.exhausted).then(|| page.next_cursor.clone());
        for (key, value) in page.entries {
            if key.index_id == index_id
                && value
                    .shadow_slot
                    .is_some_and(|shadow| shadow.index_version as u64 == target)
            {
                updates.push((key, value.shadow_slot.expect("shadow slot checked")));
            }
        }
        if page.exhausted {
            exhausted = true;
            break;
        }
    }

    for (key, shadow) in updates {
        if let Some(mut entry) = subject_store::get(&key).map_err(VectorCanisterError::from)? {
            entry.slot = Some(shadow);
            entry.shadow_slot = None;
            subject_store::insert(key, entry).map_err(VectorCanisterError::from)?;
        }
    }

    Ok((if exhausted { None } else { scan_cursor }, exhausted))
}

/// Stage 1 of `Aborting`: clear `shadow_slot@target` for up to `max_work` subjects without
/// touching `slot` or the reverse-map locator. Returns `(next_cursor, exhausted)`.
fn clear_shadow_slots(
    index_id: u32,
    target: u64,
    cursor: Option<SubjectScanCursor>,
    max_work: u32,
) -> Result<(Option<SubjectScanCursor>, bool), VectorCanisterError> {
    let mut keys: Vec<SubjectKey> = Vec::new();
    let scope = SubjectScanScope::Aborting {
        index_id,
        target_index_version: target,
    };
    let mut scan_cursor = cursor;
    let mut exhausted = false;
    for _ in 0..max_work.max(1) {
        let Some(next) = next_subject_page(scope, scan_cursor.clone(), 1)? else {
            exhausted = true;
            break;
        };
        let page = next.page;
        scan_cursor = (!page.exhausted).then(|| page.next_cursor.clone());
        for (key, value) in page.entries {
            if key.index_id == index_id
                && value
                    .shadow_slot
                    .is_some_and(|shadow| shadow.index_version as u64 == target)
            {
                keys.push(key);
            }
        }
        if page.exhausted {
            exhausted = true;
            break;
        }
    }

    for key in keys {
        if let Some(mut entry) = subject_store::get(&key).map_err(VectorCanisterError::from)? {
            entry.shadow_slot = None;
            subject_store::insert(key, entry).map_err(VectorCanisterError::from)?;
        }
    }

    Ok((if exhausted { None } else { scan_cursor }, exhausted))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_REBUILD_STEP_WORK, MAX_REBUILD_TRAINING_DISTANCE_OPS, candidate_pool_cap,
        clamp_step_work, training_start_feasible,
    };
    use crate::facade::stable::rebuild_pool;

    #[test]
    fn training_start_feasible_enforces_both_bounds() {
        // Tiny config: well within both the pool-region and op budgets.
        assert!(training_start_feasible(2, 16, 16, 4));

        // Pool region (P2): a geometry that cannot host `nlist` candidate rows (pad-stride + aux)
        // plus `nlist` trained f32 centroids inside `rebuild_pool::REGION_BYTES` is rejected. With
        // `nlist = MAX_NLIST`, both the slot and centroid arrays scale with stride, so a large
        // enough stride exceeds the region budget.
        let stride = 4 * 1050u32; // dims = 1050
        let nlist = 1024u32;
        assert!(
            rebuild_pool::pool_capacity_for(stride, nlist, stride, false).is_none(),
            "fixture must exceed the pool-region budget"
        );
        assert!(!training_start_feasible(nlist, stride, stride, 1050));

        // Op budget (P1) in isolation: a small stride keeps the region budget satisfied while
        // `nlist^2 * dims` exceeds the per-iteration op budget. (`nlist` here is only used to
        // drive the pure check; the caller separately clamps `nlist <= MAX_NLIST`.)
        let nlist = 40_000u32;
        assert!(
            rebuild_pool::pool_capacity_for(4, nlist, 4, false).is_some(),
            "fixture must satisfy the pool-region budget"
        );
        // dims = 1, so `nlist^2 * dims` is just `nlist^2`.
        assert!(
            nlist as u64 * nlist as u64 > MAX_REBUILD_TRAINING_DISTANCE_OPS,
            "fixture must exceed the op budget"
        );
        assert!(!training_start_feasible(nlist, 4, 4, 1));
    }

    #[test]
    fn candidate_pool_cap_is_at_least_nlist_when_feasible() {
        // For any params accepted by `training_start_feasible`, the pool can hold `>= nlist`
        // candidates (so sampling can reach `Training` rather than always failing). Symmetric
        // strides model an F32 index; the asymmetric pair models an I8 index whose native pool
        // width is a quarter of the f32 centroid width.
        for (nlist, pool_stride, centroid_stride, dims) in [
            (2u32, 16u32, 16u32, 4u16),
            (16, 512, 512, 128),
            (64, 3072, 3072, 768),
            (64, 768, 3072, 768),
        ] {
            assert!(training_start_feasible(
                nlist,
                pool_stride,
                centroid_stride,
                dims
            ));
            assert!(
                candidate_pool_cap(nlist, pool_stride, centroid_stride, dims) >= nlist as usize,
                "pool cap below nlist for ({nlist}, {pool_stride}, {centroid_stride}, {dims})"
            );
        }
    }

    #[test]
    fn candidate_pool_cap_matches_region_slot_capacity() {
        // The sampling policy cap and the physical region capacity are the same number whenever
        // the byte budget binds, so graceful cap-stop can never overrun the region array.
        for (nlist, pool_stride, centroid_stride, dims) in [
            (2u32, 16u32, 16u32, 4u16),
            (64, 3072, 3072, 768),
            (64, 1536, 6144, 1536),
        ] {
            if !training_start_feasible(nlist, pool_stride, centroid_stride, dims) {
                continue;
            }
            let region_cap =
                rebuild_pool::pool_capacity_for(pool_stride, nlist, centroid_stride, false)
                    .expect("cap");
            let ops_cap = MAX_REBUILD_TRAINING_DISTANCE_OPS
                / ((nlist as u64).max(1) * u64::from(dims).max(1)).max(1);
            if region_cap <= ops_cap {
                assert_eq!(
                    candidate_pool_cap(nlist, pool_stride, centroid_stride, dims) as u64,
                    region_cap,
                    "byte-bound cap must equal the physical slot capacity"
                );
            }
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
