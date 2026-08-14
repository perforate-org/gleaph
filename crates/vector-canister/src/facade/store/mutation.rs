//! `vector_upsert` / `vector_remove` over the degenerate `ivf_flat` page store.
//!
//! Idempotence is decided **only** by `mutation_id` against the retained subject clock
//! (`VECTOR_SUBJECT_TO_ID`), never by comparing stored bytes — except the single
//! same-version-different-payload conflict guard on a live row. See ADR 0031 Slice 2.

use super::rebuild::rebuild_state_of;
use super::search::{assign_partition, normalize_f32, read_centroids_at};
use super::{
    DEFAULT_MAX_PAGE_BYTES, DEGENERATE_PARTITION_ID, INITIAL_INDEX_VERSION, VectorCanisterStore,
    VectorSyncBatchOutcomeOperationError,
};
use crate::encoding::EncodingRecord;
#[cfg(test)]
use crate::facade::stable::VECTOR_PARTITION_HEADS;
use crate::facade::stable::definition_store::{self, DefinitionStoreError};
use crate::facade::stable::subject_store::{self, SubjectStoreError};
use crate::facade::stable::{
    IVF_CENTROID_META, PAGE_STORE, SHARD_CANISTER_CATALOG, VECTOR_DELETED_SUBJECTS,
};
#[cfg(test)]
use crate::records::PartitionKey;
use crate::records::{
    DeletedSubjectKey, FixedSubjectMapEntry, IvfCentroidMeta, SlotRef, SubjectKey, VectorIndexDef,
    VectorRebuildStateRecord,
};
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorEmbeddingSyncOp, VectorEncoding, VectorIndexKind, VectorMetric,
    VectorSubject, quantize_f32_to_i8,
};
use ic_stable_vector_page_store::{MAX_RUNS, PageLayout};

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

#[cfg(test)]
thread_local! {
    /// Test-only fault-injection seam for [`VectorCanisterStore::insert_subject_entry`], mirroring the
    /// page-store append seam. `None` disables injection; `Some(k)` lets the next `k` subject-map
    /// inserts succeed and forces the `(k+1)`-th to fail with
    /// [`VectorCanisterError::StableGrowFailed`] (then disarms). This exercises the fallible commit
    /// branch of mutation and rebuild subject-link commits, otherwise only reachable by exhausting
    /// stable memory.
    static FAIL_SUBJECT_INSERT_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// Arms the [`insert_subject_entry`](VectorCanisterStore::insert_subject_entry) failure seam: `skip`
/// subsequent subject-map inserts succeed, then the next one fails once with
/// [`VectorCanisterError::StableGrowFailed`]. Test-only.
#[cfg(test)]
pub(crate) fn arm_subject_insert_failure(skip: u32) {
    FAIL_SUBJECT_INSERT_AFTER.with(|c| c.set(Some(skip)));
}

#[cfg(test)]
fn take_injected_subject_insert_failure() -> bool {
    FAIL_SUBJECT_INSERT_AFTER.with(|c| match c.get() {
        Some(0) => {
            c.set(None);
            true
        }
        Some(k) => {
            c.set(Some(k - 1));
            false
        }
        None => false,
    })
}

/// Returns the `(bytes, aux)` to store for `def`: cosine unit-normalizes (rejecting zero-norm), and
/// `I8` quantizes to an i8 payload with a per-row scale in aux `[0..4]` (Model Y: the ingest bytes are
/// always F32). `F32` stores bytes verbatim with zero aux.
fn prepare_for_metric(
    def: &VectorIndexDef,
    bytes: &[u8],
) -> Result<(Vec<u8>, [u8; 8]), VectorCanisterError> {
    let normalized = if def.metric == VectorMetric::Cosine {
        normalize_f32(bytes, def.dims as usize).ok_or(VectorCanisterError::InvalidQueryVector)?
    } else {
        bytes.to_vec()
    };
    match def.encoding {
        VectorEncoding::F32 => Ok((normalized, [0u8; 8])),
        VectorEncoding::I8 => {
            let q = quantize_f32_to_i8(&normalized, def.dims as usize)?;
            let mut aux = [0u8; 8];
            aux[0..4].copy_from_slice(&q.scale.to_le_bytes());
            Ok((q.bytes, aux))
        }
    }
}

/// How a mutation must mirror its live effect across index versions during a rebuild (ADR 0031
/// Slice 7). Derived per-op from the durable rebuild state of `op.index_id`.
#[derive(Clone, Copy, Debug)]
enum RebuildMutationMode {
    /// No rebuild, or a phase with no shadow version yet / no longer (`Idle`/`Sampling`/`Training`/
    /// `Failed`/`Aborting`): operate only on the active version via `current_slot_for(active)`.
    /// Mutations during `Training` are active-only and are later shadowed when `Building` walks every
    /// live subject (ADR 0031 Slice 8).
    ActiveOnly,
    /// `Building`/`ReadyToPublish`: mirror the live effect into both the active and the shadow
    /// (`target`) version so the shadow stays publish-complete.
    DualWrite { target: u64, target_nlist: u32 },
    /// Post-publish `Cleaning`: the active version is already `target`; operate active-only via
    /// `current_slot_for(active)`. State-changing mutations collapse the touched subject
    /// (`slot = target, shadow = None`); pure idempotent no-ops are left to cleanup.
    Cleaning,
}

/// Resolves the per-op rebuild mutation mode from the durable rebuild state.
fn rebuild_mutation_mode(index_id: u32) -> RebuildMutationMode {
    match rebuild_state_of(index_id) {
        VectorRebuildStateRecord::Building {
            target_index_version,
            nlist,
            ..
        }
        | VectorRebuildStateRecord::ReadyToPublish {
            target_index_version,
            nlist,
        } => RebuildMutationMode::DualWrite {
            target: target_index_version,
            target_nlist: nlist,
        },
        VectorRebuildStateRecord::Cleaning { .. } => RebuildMutationMode::Cleaning,
        VectorRebuildStateRecord::Idle
        | VectorRebuildStateRecord::Sampling { .. }
        | VectorRebuildStateRecord::Training { .. }
        | VectorRebuildStateRecord::Aborting { .. }
        | VectorRebuildStateRecord::Failed { .. } => RebuildMutationMode::ActiveOnly,
    }
}

/// Computes `slots_per_page` from a page byte budget and stride, rejecting a `< 1` capacity.
/// Largest `slots_per_page` (capacity) whose page fits `max_page_bytes` under the new two-table page
/// geometry (ADR 0064 §7). Fails closed with [`VectorCanisterError::InvalidPageCapacity`] when even a
/// single-row page does not fit.
fn slots_per_page_for(
    max_page_bytes: u32,
    pad_stride_bytes: u32,
    meta_stride_bytes: u32,
    run_capacity: u32,
) -> Result<u32, VectorCanisterError> {
    let capacity = PageLayout::max_capacity_for(
        max_page_bytes as usize,
        pad_stride_bytes,
        meta_stride_bytes,
        run_capacity,
    )
    .ok_or(VectorCanisterError::InvalidPageCapacity)?;
    Ok(capacity)
}

/// Run-table width for a def: `min(owned_shards, MAX_RUNS)`, floored at 1. Owned shards come from the
/// shard↔canister catalog at def creation; the value is frozen into the def (ADR 0064 §7).
fn owned_run_capacity() -> u32 {
    let owned = SHARD_CANISTER_CATALOG.with_borrow(|c| c.owned_shard_count());
    (owned as u32).clamp(1, MAX_RUNS)
}

/// The deleted-list key for a tombstoned `subject` at `stamp`.
fn deleted_key_for(subject: SubjectKey, stamp: u64) -> DeletedSubjectKey {
    DeletedSubjectKey::new(subject.subject.shard_id(), stamp, subject)
}

/// Records `subject` as deleted at `stamp` in `VECTOR_DELETED_SUBJECTS` (the GC's stable cursor).
fn mark_deleted(subject: SubjectKey, stamp: u64) {
    VECTOR_DELETED_SUBJECTS.with_borrow_mut(|m| {
        m.insert(deleted_key_for(subject, stamp), 0);
    });
}

/// Removes `subject` from `VECTOR_DELETED_SUBJECTS` (on resurrect or a stamp change while deleted).
fn unmark_deleted(subject: SubjectKey, stamp: u64) {
    VECTOR_DELETED_SUBJECTS.with_borrow_mut(|m| {
        m.remove(&deleted_key_for(subject, stamp));
    });
}

impl VectorCanisterStore {
    /// Asserts the caller may mutate `subject_shard`.
    ///
    /// The Router is the trusted coordinator (ADR 0064 §6): it persists ops for any shard, so it owns
    /// every subject. A graph shard may only mutate its own shard.
    fn assert_caller_owns_subject(
        &self,
        caller: Principal,
        subject_shard: gleaph_graph_kernel::federation::ShardId,
    ) -> Result<(), VectorCanisterError> {
        let router = crate::facade::stable::VECTOR_INDEX_ROUTER.with_borrow(|r| *r.get());
        if caller == router {
            return Ok(());
        }
        let attached = crate::facade::stable::SHARD_CANISTER_CATALOG
            .with_borrow(|c| c.shard_for_canister(caller));
        let Some(shard) = attached else {
            return Err(VectorCanisterError::ShardNotAttached);
        };
        if shard != subject_shard {
            return Err(VectorCanisterError::ShardMismatch);
        }
        Ok(())
    }

    /// Returns the existing def, or lazily creates a degenerate `ivf_flat` def for an upsert.
    ///
    /// Slice 2 has no admin create-index endpoint; `kind`/`metric` have a single variant each, so a
    /// def created from the first op's `encoding`/`dims` is lossless. The Router will own definition
    /// creation in a later slice.
    fn ensure_def_for_upsert(
        &self,
        index_id: u32,
        encoding: VectorEncoding,
        dims: u16,
        metric: VectorMetric,
    ) -> Result<VectorIndexDef, VectorCanisterError> {
        self.ensure_def_for_outcome(index_id, encoding, dims, metric)
            .map_err(|error| match error {
                VectorSyncBatchOutcomeOperationError::TablePressure
                | VectorSyncBatchOutcomeOperationError::SubjectTablePressure
                | VectorSyncBatchOutcomeOperationError::StoreUnavailable
                | VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable => {
                    super::legacy_definition_store_error(DefinitionStoreError::TablePressure)
                }
                VectorSyncBatchOutcomeOperationError::Fatal(error) => error,
            })
    }

    /// Admits a lazy definition before the typed batch path writes any vector rows.
    ///
    /// Only an insertion-time `TablePressure` is terminal.  Read-side and other mutation failures
    /// are deliberately availability failures: the outer outcome has no error payload with which
    /// to acknowledge a possibly ambiguous operation.
    fn ensure_def_for_outcome(
        &self,
        index_id: u32,
        encoding: VectorEncoding,
        dims: u16,
        metric: VectorMetric,
    ) -> Result<VectorIndexDef, VectorSyncBatchOutcomeOperationError> {
        let existing = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _get_scope = bench_scope("sync_def_get");
            definition_store::get(index_id)
                .map_err(|_| VectorSyncBatchOutcomeOperationError::StoreUnavailable)?
        };
        if let Some(def) = existing {
            if def.metric != metric {
                return Err(VectorSyncBatchOutcomeOperationError::Fatal(
                    VectorCanisterError::MetricMismatch,
                ));
            }
            return Ok(def);
        }
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _create_scope = bench_scope("sync_def_create");
        // The encoding record is the single source of width truth (ADR 0064 §8): it validates the
        // (encoding, dims, metric) combination and derives the stored stride before any def or row
        // is written. `dimension_mismatch` maps any invalid combination (wire-unchanged).
        let record = EncodingRecord::from_parts(encoding, dims).map_err(|_| {
            VectorSyncBatchOutcomeOperationError::Fatal(VectorCanisterError::DimensionMismatch)
        })?;
        let stride_bytes = record.stride_bytes;
        let pad_stride_bytes = record.pad_stride_bytes;
        let meta_stride_bytes = record.meta_stride();
        let run_capacity = owned_run_capacity();
        let slots_per_page = slots_per_page_for(
            DEFAULT_MAX_PAGE_BYTES,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
        )
        .map_err(VectorSyncBatchOutcomeOperationError::Fatal)?;
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric,
            nlist: 1,
            active_index_version: INITIAL_INDEX_VERSION,
            stride_bytes,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            slots_per_page,
        };
        definition_store::insert(index_id, def)
            .map(|_| ())
            .map_err(|error| match error {
                DefinitionStoreError::TablePressure => {
                    VectorSyncBatchOutcomeOperationError::TablePressure
                }
                DefinitionStoreError::Unavailable(_) | DefinitionStoreError::Mutation(_) => {
                    VectorSyncBatchOutcomeOperationError::StoreUnavailable
                }
                #[cfg(any(test, feature = "canbench"))]
                DefinitionStoreError::Reset(_) => {
                    VectorSyncBatchOutcomeOperationError::StoreUnavailable
                }
            })?;
        IVF_CENTROID_META.with_borrow_mut(|meta| meta.insert(index_id, IvfCentroidMeta::default()));
        Ok(def)
    }

    /// Applies one operation for the additive typed batch endpoint.
    ///
    /// The definition admission happens after caller validation and before the legacy row path.  A
    /// successful admission is visible to that legacy path as an existing definition, so this
    /// method never maps CHM/page-store errors to `TablePressure`.
    pub(crate) fn vector_sync_batch_outcome_apply_one(
        &self,
        caller: Principal,
        op: &VectorEmbeddingSyncOp,
    ) -> Result<(), VectorSyncBatchOutcomeOperationError> {
        self.assert_caller_owns_subject(caller, op.subject.shard_id())
            .map_err(VectorSyncBatchOutcomeOperationError::Fatal)?;

        if op.remove {
            // A remove has no lazy definition to create, but it must establish that the owner is
            // readable before it can write the subject tombstone clock.
            let active = definition_store::get(op.index_id)
                .map_err(|_| VectorSyncBatchOutcomeOperationError::StoreUnavailable)?;
            return self
                .vector_remove_after_definition_admission(
                    op,
                    active.map(|def| def.active_index_version),
                )
                .map_err(VectorSyncBatchOutcomeOperationError::Fatal);
        }

        // Reject a malformed wire vector before the lazy definition insert. A public typed batch
        // must never classify invalid bytes as an availability result or leave a definition behind
        // in a host-side test seam; on the IC, later fatal errors still trap and roll back.
        if op.bytes.len() != op.dims as usize * 4 {
            return Err(VectorSyncBatchOutcomeOperationError::Fatal(
                VectorCanisterError::ByteWidthMismatch,
            ));
        }
        let def = self.ensure_def_for_outcome(op.index_id, op.encoding, op.dims, op.metric)?;
        let key = SubjectKey::new(op.index_id, op.subject);
        // Reserve a brand-new subject-map slot before any slab append. This makes an insertion-time
        // LHM TablePressure an exact terminal result instead of an ambiguous post-row failure.
        let reserved_new_subject = subject_store::get(&key)
            .map_err(|error| match error {
                SubjectStoreError::TablePressure => {
                    VectorSyncBatchOutcomeOperationError::SubjectTablePressure
                }
                _ => VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable,
            })?
            .is_none();
        if reserved_new_subject {
            subject_store::insert(
                key,
                FixedSubjectMapEntry {
                    stamp: op.mutation_id,
                    deleted: true,
                    slot: None,
                    shadow_slot: None,
                },
            )
            .map_err(|error| match error {
                SubjectStoreError::TablePressure => {
                    VectorSyncBatchOutcomeOperationError::SubjectTablePressure
                }
                _ => VectorSyncBatchOutcomeOperationError::SubjectStoreUnavailable,
            })?;
        }
        self.vector_upsert_after_definition_admission(op, def, reserved_new_subject)
            .map_err(VectorSyncBatchOutcomeOperationError::Fatal)
    }

    /// Appends a vector row into the given partition's page chain via the slab page store
    /// ([`crate::facade::stable::page_store`], ADR 0064 §7), rolling a new page when the mutable page
    /// is full or its run table would overflow. Fallible because the slab can fail to `grow`; the
    /// store commits write-then-commit so a failed grow leaves no head/meta pointing at unwritten
    /// bytes.
    ///
    /// Production callers pass `DEGENERATE_PARTITION_ID` (every production def is `nlist == 1`); the
    /// `partition_id` parameter is what lets the Slice 6 seed helpers populate `nlist > 1` partition
    /// chains and is forward-useful for the Slice 7 rebuild.
    pub(super) fn append_slot(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        def: &VectorIndexDef,
        subject: VectorSubject,
        bytes: &[u8],
    ) -> Result<SlotRef, VectorCanisterError> {
        let (stored, aux) = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _prepare_scope = bench_scope("sync_prepare");
            prepare_for_metric(def, bytes)?
        };
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _append_scope = bench_scope("sync_append_row");
        PAGE_STORE.with_borrow_mut(|store| {
            store.append_row(
                index_id,
                index_version,
                partition_id,
                def,
                subject,
                &stored,
                &aux,
            )
        })
    }

    /// Batched append used by the rebuild shadow build: appends already-stored rows (payload + aux)
    /// via [`VectorSlabStore::append_rows`], which commits the page directory at page granularity
    /// instead of per row. This is a **carry-forward** path: the caller passes the stored `(bytes,
    /// aux)` as-is and it is written verbatim (no re-quantize), so a rebuilt `I8` row is never
    /// double-quantized. Returns one `SlotRef` per input row, in order.
    pub(super) fn append_slot_batch(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        def: &VectorIndexDef,
        rows: &[(VectorSubject, &[u8], [u8; 8])],
    ) -> Result<Vec<SlotRef>, VectorCanisterError> {
        PAGE_STORE.with_borrow_mut(|store| {
            store.append_rows(index_id, index_version, partition_id, def, rows)
        })
    }

    /// Marks a slot tombstoned via the slab page store, which owns the `VectorPageMeta` live/
    /// tombstone counts and the `VECTOR_PARTITION_HEADS.live_len` decrement. Idempotent.
    pub(super) fn tombstone_slot(&self, index_id: u32, slot: SlotRef) {
        PAGE_STORE.with_borrow_mut(|store| {
            store.tombstone_row(index_id, slot);
        });
    }

    pub(super) fn read_slot_bytes(&self, index_id: u32, slot: SlotRef) -> Option<Vec<u8>> {
        PAGE_STORE.with_borrow(|store| {
            store
                .read_row_bytes(index_id, slot)
                .map(|(_, bytes, _aux)| bytes)
        })
    }

    /// Inserts a subject-map row, mapping the map's `OutOfMemory` to the canister's stable-grow
    /// error so mutation and rebuild callers can propagate it instead of panicking.
    pub(super) fn insert_subject_entry(
        &self,
        key: SubjectKey,
        entry: FixedSubjectMapEntry,
    ) -> Result<(), VectorCanisterError> {
        // Test-only: simulate a stable-memory grow failure before the map insert (see seam above).
        #[cfg(test)]
        if take_injected_subject_insert_failure() {
            return Err(VectorCanisterError::StableGrowFailed);
        }
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("sync_subject_insert");
        subject_store::insert(key, entry)
            .map(|_| ())
            .map_err(super::legacy_subject_store_error)
    }

    /// Partition for an append on the **active** version: degenerate partition `0` when `nlist <= 1`,
    /// otherwise the nearest active centroid (ADR 0031 Slice 6/7). A missing/incomplete active
    /// centroid set falls back to partition `0` (the same fail-soft the search path uses). This is
    /// what makes a published `nlist > 1` index mutable.
    fn active_partition(&self, def: &VectorIndexDef, index_id: u32, bytes: &[u8]) -> u32 {
        if def.nlist <= 1 {
            return DEGENERATE_PARTITION_ID;
        }
        match read_centroids_at(index_id, def.active_index_version, def.nlist, def.dims) {
            Some(centroids) => assign_partition(&centroids, bytes),
            None => DEGENERATE_PARTITION_ID,
        }
    }

    /// Partition for an append into the rebuild's **shadow** (`target`) version: nearest target
    /// centroid (the shadow always has `nlist > 1` ready centroids by construction).
    fn shadow_partition(
        &self,
        index_id: u32,
        target: u64,
        target_nlist: u32,
        dims: u16,
        bytes: &[u8],
    ) -> u32 {
        match read_centroids_at(index_id, target, target_nlist, dims) {
            Some(centroids) => assign_partition(&centroids, bytes),
            None => DEGENERATE_PARTITION_ID,
        }
    }

    /// Applies an upsert, ordered by the single `mutation_id` stamp against the retained subject
    /// clock (ADR 0064 §5):
    ///
    /// - **Older incarnation** (`op.inc < clock.inc`): stale no-op — a stale replay can never
    ///   resurrect or mutate a subject whose identity has already moved on.
    /// - **Newer incarnation** (`op.inc > clock.inc`): **resurrect** with a *fresh* `VectorId`. This
    ///   is the only resurrection path; it requires a strictly greater incarnation, which the graph
    ///   canonical store allocates on each delete/reinsert. Any live slot of the older incarnation is
    ///   tombstoned first so it cannot orphan.
    /// - **Same incarnation** (`op.inc == clock.inc`): version rules within the incarnation. If the
    ///   subject is already deleted at this stamp the upsert is a stale replay (no-op, since a
    ///   genuine reinsert carries a greater stamp). On a live subject: stale `<` no-op; `==`
    ///   identical no-op / different `MutationStampConflict`; `>` appends a new slot.
    pub fn vector_upsert(
        &self,
        caller: Principal,
        op: &VectorEmbeddingSyncOp,
    ) -> Result<(), VectorCanisterError> {
        if op.remove {
            return Err(VectorCanisterError::MutationKindMismatch);
        }
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _caller_scope = bench_scope("sync_caller_check");
            self.assert_caller_owns_subject(caller, op.subject.shard_id())?;
        }
        let def = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _def_scope = bench_scope("sync_def_ensure");
            self.ensure_def_for_upsert(op.index_id, op.encoding, op.dims, op.metric)?
        };
        self.vector_upsert_after_definition_admission(op, def, false)
    }

    /// Executes an upsert after the caller and definition admission checks completed.
    fn vector_upsert_after_definition_admission(
        &self,
        op: &VectorEmbeddingSyncOp,
        def: VectorIndexDef,
        reserved_new_subject: bool,
    ) -> Result<(), VectorCanisterError> {
        if op.encoding != def.encoding || op.dims != def.dims {
            return Err(VectorCanisterError::DimensionMismatch);
        }
        // Model Y: the wire embedding bytes are always canonical F32 (`dims * 4`), independent of the
        // stored encoding. `def.stride_bytes` is the stored width (`dims` for I8), which must NOT be
        // used here.
        if op.bytes.len() != op.dims as usize * 4 {
            return Err(VectorCanisterError::ByteWidthMismatch);
        }
        let active = def.active_index_version;
        let mode = rebuild_mutation_mode(op.index_id);
        let key = SubjectKey::new(op.index_id, op.subject);
        let existing = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _get_scope = bench_scope("sync_subject_get");
            subject_store::get(&key).map_err(super::legacy_subject_store_error)?
        };

        let Some(entry) = existing else {
            // New subject: allocate a fresh VectorId and create a live slot.
            self.insert_new_subject(op, &def, mode, key, false)?;
            return Ok(());
        };

        if reserved_new_subject
            && entry.deleted
            && entry.slot.is_none()
            && entry.stamp == op.mutation_id
        {
            self.insert_new_subject(op, &def, mode, key, true)?;
            return Ok(());
        }

        match op.mutation_id.cmp(&entry.stamp) {
            std::cmp::Ordering::Less => Ok(()), // stale replay: no-op
            std::cmp::Ordering::Greater => {
                // Newer stamp: append a fresh slot (and shadow while dual-writing) FIRST (fallible),
                // then commit the subject map pointing at the new slot (fallible), then tombstone any
                // live slot of the older stamp (infallible). Write-then-commit holds end-to-end: a
                // failed shadow append or a failed subject-map commit both leave the old slot live,
                // because the old slot is only tombstoned after the commit succeeds
                // (GAP-2026-08-07-001).
                let active_partition = self.active_partition(&def, op.index_id, &op.bytes);
                let new_slot = self.append_slot(
                    op.index_id,
                    active,
                    active_partition,
                    &def,
                    op.subject,
                    &op.bytes,
                )?;
                let shadow_slot = match mode {
                    RebuildMutationMode::DualWrite {
                        target,
                        target_nlist,
                    } => {
                        let partition = self.shadow_partition(
                            op.index_id,
                            target,
                            target_nlist,
                            def.dims,
                            &op.bytes,
                        );
                        match self.append_slot(
                            op.index_id,
                            target,
                            partition,
                            &def,
                            op.subject,
                            &op.bytes,
                        ) {
                            Ok(shadow) => Some(shadow),
                            Err(err) => {
                                self.tombstone_slot(op.index_id, new_slot);
                                return Err(err);
                            }
                        }
                    }
                    RebuildMutationMode::ActiveOnly | RebuildMutationMode::Cleaning => None,
                };
                if let Err(err) = self.insert_subject_entry(
                    key,
                    FixedSubjectMapEntry {
                        stamp: op.mutation_id,
                        deleted: false,
                        slot: Some(new_slot),
                        shadow_slot,
                    },
                ) {
                    // The subject-map commit failed (OutOfMemory): tombstone the just-appended slots so
                    // the residual is a tombstoned dead row (live counters restored) rather than a
                    // live-counted orphan. The old slot is NOT touched here (it is only tombstoned
                    // after a successful commit), so the retained old entry keeps pointing at a live
                    // row — the GAP-2026-08-07-001 fix.
                    self.tombstone_slot(op.index_id, new_slot);
                    if let Some(shadow) = shadow_slot {
                        self.tombstone_slot(op.index_id, shadow);
                    }
                    return Err(err);
                }
                // Infallible: the commit succeeded, so now tombstone the superseded live slots
                // (active, and the old shadow while dual-writing).
                if !entry.deleted {
                    if let Some(active_slot) = entry.current_slot_for(active) {
                        self.tombstone_slot(op.index_id, active_slot);
                    }
                    if let RebuildMutationMode::DualWrite { .. } = mode
                        && let Some(old_shadow) = entry.shadow_slot
                    {
                        self.tombstone_slot(op.index_id, old_shadow);
                    }
                } else {
                    // Resurrect: the subject was tombstoned at `entry.stamp`; drop it from the
                    // deleted list so the GC does not remove the now-live row (only after the commit
                    // succeeded, so a failed resurrection keeps the deleted marking).
                    unmark_deleted(key, entry.stamp);
                }
                Ok(())
            }
            std::cmp::Ordering::Equal => {
                if entry.deleted {
                    // Same stamp already tombstoned: a genuine reinsert would carry a greater stamp,
                    // so this is a stale replay.
                    return Ok(());
                }
                // Same stamp on a live subject: idempotent no-op if the stored payload matches the
                // expected stored form of `op.bytes`, else a conflict. The expected form is exactly
                // what `append_slot` stored: cosine-normalized and, for I8, quantized. Both are
                // deterministic, so a byte-identical replay matches; a different payload conflicts.
                let slot = entry
                    .current_slot_for(active)
                    .expect("live entry has a slot");
                let stored = self.read_slot_bytes(op.index_id, slot).unwrap_or_default();
                let expected = prepare_for_metric(&def, &op.bytes)?.0;
                // Compare only the meaningful stored payload (`stride_bytes`); the page row carries
                // trailing pad, so the stored slice is wider than `expected` for I8.
                let same = stored.get(..def.stride_bytes as usize) == Some(expected.as_slice());
                if same {
                    // Pure idempotent no-op: nothing changes. During `Cleaning` this intentionally
                    // does *not* collapse `shadow_slot -> slot` (collapse-on-touch only applies to
                    // state-changing mutations); search stays correct via `current_slot_for` and
                    // the subject is collapsed later by `cleanup_step`.
                    return Ok(());
                }
                Err(VectorCanisterError::MutationStampConflict)
            }
        }
    }

    /// Inserts a brand-new (or resurrected) live subject. The active row is assigned to its active
    /// partition; while dual-writing, a mirror row is also appended into the shadow `target` version
    /// and recorded in `shadow_slot` (ADR 0031 Slice 7).
    fn insert_new_subject(
        &self,
        op: &VectorEmbeddingSyncOp,
        def: &VectorIndexDef,
        mode: RebuildMutationMode,
        key: SubjectKey,
        reserved: bool,
    ) -> Result<(), VectorCanisterError> {
        let active = def.active_index_version;
        let active_partition = self.active_partition(def, op.index_id, &op.bytes);
        let slot = self.append_slot(
            op.index_id,
            active,
            active_partition,
            def,
            op.subject,
            &op.bytes,
        )?;
        // Append the shadow mirror (while dual-writing) BEFORE committing the subject map so a
        // fallible slab grow cannot orphan it against a missing shadow row. On shadow failure
        // we tombstone the just-appended active row before returning, so the residual is a tombstoned
        // dead row (live counters restored) rather than a live-counted orphan; the subject map
        // stays untouched.
        let shadow_slot = match mode {
            RebuildMutationMode::DualWrite {
                target,
                target_nlist,
            } => {
                let partition =
                    self.shadow_partition(op.index_id, target, target_nlist, def.dims, &op.bytes);
                match self.append_slot(op.index_id, target, partition, def, op.subject, &op.bytes) {
                    Ok(shadow) => Some(shadow),
                    Err(err) => {
                        self.tombstone_slot(op.index_id, slot);
                        return Err(err);
                    }
                }
            }
            RebuildMutationMode::ActiveOnly | RebuildMutationMode::Cleaning => None,
        };
        let entry = FixedSubjectMapEntry {
            stamp: op.mutation_id,
            deleted: false,
            slot: Some(slot),
            shadow_slot,
        };
        let insert_result = if reserved {
            subject_store::insert(key, entry)
                .map(|_| ())
                .map_err(super::legacy_subject_store_error)
        } else {
            self.insert_subject_entry(key, entry)
        };
        if let Err(err) = insert_result {
            // The subject-map commit failed (OutOfMemory): tombstone the just-appended slots so the
            // residual is a tombstoned dead row (live counters restored) rather than a live-counted
            // orphan, mirroring the shadow-append failure handling above.
            self.tombstone_slot(op.index_id, slot);
            if let Some(shadow) = shadow_slot {
                self.tombstone_slot(op.index_id, shadow);
            }
            return Err(err);
        }
        Ok(())
    }

    /// Applies a remove, ordered by the single `mutation_id` stamp against the retained subject
    /// clock (ADR 0064 §5):
    ///
    /// - **Older incarnation** (`op.inc < clock.inc`): stale no-op. This closes the reverse-orphan
    ///   race — a late repair-drain remove for a deleted incarnation can never tombstone a newer
    ///   reinsert that already advanced the clock.
    /// - **Newer incarnation** (`op.inc > clock.inc`): authoritative remove for an as-yet-unseen
    ///   incarnation; tombstone any live slot and record the deleted clock at the op's incarnation.
    /// - **Same incarnation** (`op.inc == clock.inc`): stale `<` version no-op; on a deleted subject
    ///   bump the clock if `>`; on a live subject tombstone the active slot.
    ///
    /// A `remove` for a never-inserted subject still **writes a tombstone clock** (not a pure no-op).
    /// The clock no longer *blocks* resurrection by itself: a delivered upsert with a greater
    /// incarnation resurrects (see [`Self::vector_upsert`]). Stale-replay protection is the
    /// incarnation fence plus the graph repair-drain's canonical re-derivation
    /// ([`crate::index::repair_journal`]); a canonical-wins removal arrives with an authoritative
    /// (maximum) `mutation_id` so it supersedes any live slot of the same stamp.
    pub fn vector_remove(
        &self,
        caller: Principal,
        op: &VectorEmbeddingSyncOp,
    ) -> Result<(), VectorCanisterError> {
        if !op.remove {
            return Err(VectorCanisterError::MutationKindMismatch);
        }
        self.assert_caller_owns_subject(caller, op.subject.shard_id())?;
        let active = definition_store::get(op.index_id)
            .map_err(super::legacy_definition_store_error)?
            .map(|def| def.active_index_version);
        self.vector_remove_after_definition_admission(op, active)
    }

    /// Executes a remove after caller and definition-store readability are established.
    fn vector_remove_after_definition_admission(
        &self,
        op: &VectorEmbeddingSyncOp,
        active: Option<u64>,
    ) -> Result<(), VectorCanisterError> {
        let mode = rebuild_mutation_mode(op.index_id);
        let key = SubjectKey::new(op.index_id, op.subject);
        let existing = subject_store::get(&key).map_err(super::legacy_subject_store_error)?;

        let Some(entry) = existing else {
            self.insert_subject_entry(
                key,
                FixedSubjectMapEntry {
                    stamp: op.mutation_id,
                    deleted: true,
                    slot: None,
                    shadow_slot: None,
                },
            )?;
            mark_deleted(key, op.mutation_id);
            return Ok(());
        };

        // Live active slot resolved against the active version (`shadow_slot` once published into
        // `Cleaning`); falls back to `entry.slot` only if the def somehow vanished.
        let active_live_slot = active
            .and_then(|a| entry.current_slot_for(a))
            .or(entry.slot);

        match op.mutation_id.cmp(&entry.stamp) {
            std::cmp::Ordering::Less => Ok(()), // stale remove: no-op (fenced)
            std::cmp::Ordering::Greater => {
                // Authoritative remove for a newer stamp: tombstone any live slot (active, and the
                // shadow while dual-writing) and record the deleted clock at the op's stamp.
                if !entry.deleted {
                    if let Some(slot) = active_live_slot {
                        self.tombstone_slot(op.index_id, slot);
                    }
                    if let RebuildMutationMode::DualWrite { .. } = mode
                        && let Some(shadow_slot) = entry.shadow_slot
                    {
                        self.tombstone_slot(op.index_id, shadow_slot);
                    }
                } else {
                    // Already deleted at an older stamp: the deleted-list key changes with the stamp.
                    unmark_deleted(key, entry.stamp);
                }
                self.insert_subject_entry(
                    key,
                    FixedSubjectMapEntry {
                        stamp: op.mutation_id,
                        deleted: true,
                        slot: None,
                        shadow_slot: None,
                    },
                )?;
                mark_deleted(key, op.mutation_id);
                Ok(())
            }
            std::cmp::Ordering::Equal => {
                if entry.deleted {
                    // Same stamp already tombstoned: no-op.
                    return Ok(());
                }
                // Same stamp on a live subject: tombstone the active slot (and shadow while
                // dual-writing) and record the deleted clock.
                let slot = active_live_slot.expect("live entry has a slot");
                self.tombstone_slot(op.index_id, slot);
                if let RebuildMutationMode::DualWrite { .. } = mode
                    && let Some(shadow_slot) = entry.shadow_slot
                {
                    self.tombstone_slot(op.index_id, shadow_slot);
                }
                self.insert_subject_entry(
                    key,
                    FixedSubjectMapEntry {
                        stamp: op.mutation_id,
                        deleted: true,
                        slot: None,
                        shadow_slot: None,
                    },
                )?;
                mark_deleted(key, op.mutation_id);
                Ok(())
            }
        }
    }

    /// Applies a chunk of `vector_sync_batch` operations. Within a degenerate `nlist == 1` index in
    /// `ActiveOnly` mode, consecutive batchable upserts (fresh subjects, or live subjects with a newer
    /// stamp — a live `Greater` update) with distinct subjects are grouped into runs and each run is
    /// appended in one batched page-store call (amortizing the per-row page-directory commit) before
    /// its subject entries are committed per row. Ops that are not batchable (remove, a different
    /// index, a stale/equal/resurrect stamp, a subject already seen in the current run, `nlist > 1`, or
    /// dual-write) are processed per-row via `vector_upsert` / `vector_remove`.
    ///
    /// Write-then-commit holds on each batched run: a failed subject-map commit at op `k` tombstones
    /// the old slots of ops `0..k-1` (whose commits succeeded) and the new slots of ops `k..n` (whose
    /// commits did not), so no live orphan is left and the residual is tombstoned dead rows. Returns the
    /// number of ops applied (always `chunk.len()` on success).
    pub fn vector_sync_batch_chunk(
        &self,
        caller: Principal,
        chunk: &[VectorEmbeddingSyncOp],
    ) -> Result<u32, VectorCanisterError> {
        let first = &chunk[0];
        let def =
            self.ensure_def_for_upsert(first.index_id, first.encoding, first.dims, first.metric)?;
        let mode = rebuild_mutation_mode(first.index_id);

        // Non-degenerate or dual-writing indexes cannot batch any op: process the whole chunk per-row.
        if def.nlist > 1 || !matches!(mode, RebuildMutationMode::ActiveOnly) {
            for op in chunk {
                if op.remove {
                    self.vector_remove(caller, op)?;
                } else {
                    self.vector_upsert(caller, op)?;
                }
            }
            return Ok(chunk.len() as u32);
        }

        let active = def.active_index_version;
        let mut applied = 0u32;
        let mut run_rows: Vec<(VectorSubject, Vec<u8>, [u8; 8])> = Vec::new();
        let mut run_ops: Vec<&VectorEmbeddingSyncOp> = Vec::new();
        let mut run_keys: Vec<SubjectKey> = Vec::new();
        let mut run_old_slots: Vec<Option<SlotRef>> = Vec::new();
        let mut run_subjects: Vec<SubjectKey> = Vec::new();

        // Flush the current run: batch-append its rows, commit subject entries, then tombstone the
        // superseded old slots (live Greater) only after all commits succeed (write-then-commit).
        let flush = |run_rows: &mut Vec<(VectorSubject, Vec<u8>, [u8; 8])>,
                     run_ops: &mut Vec<&VectorEmbeddingSyncOp>,
                     run_keys: &mut Vec<SubjectKey>,
                     run_old_slots: &mut Vec<Option<SlotRef>>|
         -> Result<(), VectorCanisterError> {
            if run_ops.is_empty() {
                return Ok(());
            }
            let row_refs: Vec<(VectorSubject, &[u8], [u8; 8])> = run_rows
                .iter()
                .map(|(s, b, a)| (*s, b.as_slice(), *a))
                .collect();
            let slots = self.append_slot_batch(
                first.index_id,
                active,
                DEGENERATE_PARTITION_ID,
                &def,
                &row_refs,
            )?;
            for (i, op) in run_ops.iter().enumerate() {
                if let Err(err) = self.insert_subject_entry(
                    run_keys[i],
                    FixedSubjectMapEntry {
                        stamp: op.mutation_id,
                        deleted: false,
                        slot: Some(slots[i]),
                        shadow_slot: None,
                    },
                ) {
                    // Ops 0..i-1 committed: tombstone their old slots (write-then-commit allows this).
                    for old in run_old_slots[..i].iter().flatten() {
                        self.tombstone_slot(first.index_id, *old);
                    }
                    // Ops i..n did not commit: tombstone their new slots (dead rows).
                    for slot in &slots[i..] {
                        self.tombstone_slot(first.index_id, *slot);
                    }
                    return Err(err);
                }
            }
            // All commits succeeded: tombstone the superseded old slots.
            for old in run_old_slots.iter().flatten() {
                self.tombstone_slot(first.index_id, *old);
            }
            Ok(())
        };

        for op in chunk {
            if op.remove || op.index_id != first.index_id {
                // Flush the current run, then process this op per-row.
                flush(
                    &mut run_rows,
                    &mut run_ops,
                    &mut run_keys,
                    &mut run_old_slots,
                )?;
                applied = applied.saturating_add(run_ops.len() as u32);
                run_rows.clear();
                run_ops.clear();
                run_keys.clear();
                run_old_slots.clear();
                run_subjects.clear();
                if op.remove {
                    self.vector_remove(caller, op)?;
                } else {
                    self.vector_upsert(caller, op)?;
                }
                applied = applied.saturating_add(1);
                continue;
            }
            self.assert_caller_owns_subject(caller, op.subject.shard_id())?;
            if op.encoding != def.encoding || op.dims != def.dims {
                return Err(VectorCanisterError::DimensionMismatch);
            }
            if op.bytes.len() != op.dims as usize * 4 {
                return Err(VectorCanisterError::ByteWidthMismatch);
            }
            let key = SubjectKey::new(op.index_id, op.subject);
            if run_subjects.contains(&key) {
                // Repeated subject in the run: classification is order-dependent, flush and per-row.
                flush(
                    &mut run_rows,
                    &mut run_ops,
                    &mut run_keys,
                    &mut run_old_slots,
                )?;
                applied = applied.saturating_add(run_ops.len() as u32);
                run_rows.clear();
                run_ops.clear();
                run_keys.clear();
                run_old_slots.clear();
                run_subjects.clear();
                self.vector_upsert(caller, op)?;
                applied = applied.saturating_add(1);
                continue;
            }
            let existing = subject_store::get(&key).map_err(super::legacy_subject_store_error)?;
            // Classify: fresh (no entry) or live Greater (newer stamp on a live subject) are batchable.
            let old_active_slot = match &existing {
                None => None,
                Some(entry) => {
                    if op.mutation_id > entry.stamp && !entry.deleted {
                        entry.current_slot_for(active)
                    } else {
                        // Not batchable (Less, Equal, resurrect, stale): flush and process per-row.
                        flush(
                            &mut run_rows,
                            &mut run_ops,
                            &mut run_keys,
                            &mut run_old_slots,
                        )?;
                        applied = applied.saturating_add(run_ops.len() as u32);
                        run_rows.clear();
                        run_ops.clear();
                        run_keys.clear();
                        run_old_slots.clear();
                        run_subjects.clear();
                        self.vector_upsert(caller, op)?;
                        applied = applied.saturating_add(1);
                        continue;
                    }
                }
            };
            let (stored, aux) = prepare_for_metric(&def, &op.bytes)?;
            run_rows.push((op.subject, stored, aux));
            run_ops.push(op);
            run_keys.push(key);
            run_old_slots.push(old_active_slot);
            run_subjects.push(key);
        }
        flush(
            &mut run_rows,
            &mut run_ops,
            &mut run_keys,
            &mut run_old_slots,
        )?;
        applied = applied.saturating_add(run_ops.len() as u32);

        Ok(applied)
    }

    // --- Test-only inspection / setup helpers ---

    /// Creates an index def with an explicit page byte budget (test-only; production creates defs
    /// lazily on first upsert with [`DEFAULT_MAX_PAGE_BYTES`]).
    #[cfg(test)]
    pub(crate) fn create_index_for_test(
        &self,
        index_id: u32,
        encoding: VectorEncoding,
        dims: u16,
        max_page_bytes: u32,
    ) -> Result<(), VectorCanisterError> {
        let record = EncodingRecord::from_parts(encoding, dims)
            .map_err(|_| VectorCanisterError::DimensionMismatch)?;
        let stride_bytes = record.stride_bytes;
        let pad_stride_bytes = record.pad_stride_bytes;
        let meta_stride_bytes = record.meta_stride();
        let run_capacity = 1; // isolated single-shard tests
        let slots_per_page = slots_per_page_for(
            max_page_bytes,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
        )?;
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric: VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: INITIAL_INDEX_VERSION,
            stride_bytes,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
            max_page_bytes,
            slots_per_page,
        };
        definition_store::insert(index_id, def)
            .map(|_| ())
            .map_err(super::legacy_definition_store_error)?;
        IVF_CENTROID_META.with_borrow_mut(|meta| meta.insert(index_id, IvfCentroidMeta::default()));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn subject_entry_for_test(
        &self,
        index_id: u32,
        subject: gleaph_graph_kernel::vector_index::VectorSubject,
    ) -> Option<FixedSubjectMapEntry> {
        subject_store::get(&SubjectKey::new(index_id, subject))
            .ok()
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn def_for_test(&self, index_id: u32) -> Option<VectorIndexDef> {
        definition_store::get(index_id).ok().flatten()
    }

    #[cfg(test)]
    pub(crate) fn partition_head_for_test(
        &self,
        index_id: u32,
        index_version: u64,
    ) -> Option<crate::records::PartitionHead> {
        VECTOR_PARTITION_HEADS.with_borrow(|heads| {
            heads.get(&PartitionKey::new(
                index_id,
                index_version,
                DEGENERATE_PARTITION_ID,
            ))
        })
    }
}
